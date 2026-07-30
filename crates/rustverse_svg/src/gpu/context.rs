use std::fmt;

use anyhow::Context as _;

use crate::{RenderScale, scene::LogicalSize};

/// The color format shared by headless render targets and their PNG output.
pub const RGBA8_TARGET_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

/// A render target size in physical pixels.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PhysicalSize {
    pub width: u32,
    pub height: u32,
}

/// Converts a logical size to physical pixels using the requested render scale.
///
/// Dimensions are rounded to the nearest pixel, matching the renderer's
/// logical-to-physical contract. A zero or non-finite result is rejected.
pub fn physical_size(
    logical_size: LogicalSize,
    scale: RenderScale,
) -> anyhow::Result<PhysicalSize> {
    let width = scaled_dimension(logical_size.width, scale, "width")?;
    let height = scaled_dimension(logical_size.height, scale, "height")?;
    Ok(PhysicalSize { width, height })
}

fn scaled_dimension(value: f32, scale: RenderScale, name: &str) -> anyhow::Result<u32> {
    anyhow::ensure!(
        value.is_finite() && value > 0.0,
        "logical {name} must be finite and greater than zero"
    );
    let scaled = value * scale.factor();
    let rounded = scaled.round();
    anyhow::ensure!(
        scaled.is_finite() && rounded >= 1.0 && rounded <= u32::MAX as f32,
        "physical {name} is outside the supported pixel range"
    );
    Ok(rounded as u32)
}

/// A failure to initialize a headless GPU context.
#[derive(Debug)]
pub enum GpuInitError {
    /// The host has no adapter usable without a presentation surface.
    AdapterUnavailable(wgpu::RequestAdapterError),
    /// An adapter exists, but creating its logical device failed.
    DeviceUnavailable(wgpu::RequestDeviceError),
}

impl fmt::Display for GpuInitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AdapterUnavailable(error) => {
                write!(formatter, "no headless GPU adapter is available: {error}")
            }
            Self::DeviceUnavailable(error) => {
                write!(formatter, "headless GPU device creation failed: {error}")
            }
        }
    }
}

impl std::error::Error for GpuInitError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::AdapterUnavailable(error) => Some(error),
            Self::DeviceUnavailable(error) => Some(error),
        }
    }
}

/// An unpadded RGBA8 readback and its PNG encoding.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct HeadlessImage {
    pub size: PhysicalSize,
    pub rgba: Vec<u8>,
    pub png: Vec<u8>,
}

/// Reusable surface-free GPU state.
///
/// One context owns one adapter/device/queue trio and can render any number of
/// offscreen targets without recreating GPU state.
#[derive(Debug)]
pub(crate) struct GpuContext {
    _instance: wgpu::Instance,
    device: wgpu::Device,
    queue: wgpu::Queue,
}

impl GpuContext {
    /// Creates an adapter, device, and queue without creating a window surface.
    pub(crate) async fn new() -> Result<Self, GpuInitError> {
        let instance = wgpu::Instance::default();
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::None,
                force_fallback_adapter: false,
                compatible_surface: None,
                apply_limit_buckets: false,
            })
            .await
            .map_err(GpuInitError::AdapterUnavailable)?;
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("rustverse_svg headless device"),
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits::default(),
                memory_hints: wgpu::MemoryHints::MemoryUsage,
                experimental_features: wgpu::ExperimentalFeatures::disabled(),
                trace: wgpu::Trace::Off,
            })
            .await
            .map_err(GpuInitError::DeviceUnavailable)?;

        Ok(Self {
            _instance: instance,
            device,
            queue,
        })
    }

    /// Clears an offscreen RGBA8 target and returns unpadded pixels plus PNG.
    pub(crate) async fn render_clear(
        &self,
        logical_size: LogicalSize,
        scale: RenderScale,
        color: [f64; 4],
        max_target_bytes: u64,
    ) -> anyhow::Result<HeadlessImage> {
        anyhow::ensure!(
            color
                .iter()
                .all(|channel| channel.is_finite() && (0.0..=1.0).contains(channel)),
            "clear color channels must be finite and within 0..=1"
        );
        let size = physical_size(logical_size, scale)?;
        let layout = TargetLayout::validate(size, max_target_bytes, &self.device.limits())?;
        let texture_extent = wgpu::Extent3d {
            width: size.width,
            height: size.height,
            depth_or_array_layers: 1,
        };
        // All dimensions and allocation sizes are validated before either GPU
        // allocation is attempted.
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("rustverse_svg headless RGBA8 target"),
            size: texture_extent,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: RGBA8_TARGET_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::COPY_SRC
                | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());

        let readback = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("rustverse_svg headless readback"),
            size: layout.staging_bytes,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("rustverse_svg headless clear and readback"),
            });
        {
            let color_attachments = [Some(wgpu::RenderPassColorAttachment {
                view: &view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color {
                        r: color[0],
                        g: color[1],
                        b: color[2],
                        a: color[3],
                    }),
                    store: wgpu::StoreOp::Store,
                },
            })];
            let _pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("rustverse_svg headless clear pass"),
                color_attachments: &color_attachments,
                ..Default::default()
            });
        }
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(layout.padded_bytes_per_row),
                    rows_per_image: Some(size.height),
                },
            },
            texture_extent,
        );
        self.queue.submit([encoder.finish()]);

        let slice = readback.slice(..);
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        slice.map_async(wgpu::MapMode::Read, move |result| {
            let _ = sender.send(result);
        });
        self.device
            .poll(wgpu::PollType::wait_indefinitely())
            .context("waiting for GPU readback failed")?;
        receiver
            .recv()
            .context("GPU readback callback was dropped")?
            .context("mapping GPU readback buffer failed")?;

        let mapped = slice
            .get_mapped_range()
            .context("accessing mapped GPU readback buffer failed")?;
        let row_len = layout.unpadded_bytes_per_row as usize;
        let rgba_capacity =
            usize::try_from(layout.rgba_bytes).context("RGBA output is too large for this host")?;
        let mut rgba = Vec::with_capacity(rgba_capacity);
        for row in mapped.chunks_exact(layout.padded_bytes_per_row as usize) {
            rgba.extend_from_slice(&row[..row_len]);
        }
        drop(mapped);
        readback.unmap();

        let png = encode_png(size, &rgba)?;
        Ok(HeadlessImage { size, rgba, png })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TargetLayout {
    unpadded_bytes_per_row: u32,
    padded_bytes_per_row: u32,
    rgba_bytes: u64,
    staging_bytes: u64,
}

impl TargetLayout {
    fn validate(
        size: PhysicalSize,
        max_target_bytes: u64,
        limits: &wgpu::Limits,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(max_target_bytes > 0, "max_target_bytes must be non-zero");
        anyhow::ensure!(
            size.width <= limits.max_texture_dimension_2d
                && size.height <= limits.max_texture_dimension_2d,
            "physical target {}x{} exceeds max_texture_dimension_2d {}",
            size.width,
            size.height,
            limits.max_texture_dimension_2d
        );

        let unpadded_row = u64::from(size.width)
            .checked_mul(4)
            .context("RGBA row byte count overflowed")?;
        let rgba_bytes = unpadded_row
            .checked_mul(u64::from(size.height))
            .context("RGBA target byte count overflowed")?;
        let alignment = u64::from(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
        let padded_row = unpadded_row
            .checked_add(alignment - 1)
            .context("aligned staging row byte count overflowed")?
            / alignment
            * alignment;
        let staging_bytes = padded_row
            .checked_mul(u64::from(size.height))
            .context("GPU staging buffer byte count overflowed")?;
        let total_allocation_bytes = rgba_bytes
            .checked_add(staging_bytes)
            .context("combined target and staging byte count overflowed")?;

        anyhow::ensure!(
            total_allocation_bytes <= max_target_bytes,
            "target and padded staging buffer require {total_allocation_bytes} bytes \
             ({rgba_bytes} RGBA + {staging_bytes} staging), exceeding max_target_bytes \
             {max_target_bytes}"
        );
        anyhow::ensure!(
            staging_bytes <= limits.max_buffer_size,
            "padded staging buffer requires {staging_bytes} bytes, exceeding device max_buffer_size {}",
            limits.max_buffer_size
        );

        Ok(Self {
            unpadded_bytes_per_row: u32::try_from(unpadded_row)
                .context("RGBA row byte count exceeds the wgpu layout range")?,
            padded_bytes_per_row: u32::try_from(padded_row)
                .context("aligned staging row exceeds the wgpu layout range")?,
            rgba_bytes,
            staging_bytes,
        })
    }
}

fn encode_png(size: PhysicalSize, rgba: &[u8]) -> anyhow::Result<Vec<u8>> {
    let mut png = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut png, size.width, size.height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder
            .write_header()
            .context("writing PNG header failed")?;
        writer
            .write_image_data(rgba)
            .context("writing PNG image data failed")?;
    }
    Ok(png)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits(max_texture_dimension_2d: u32, max_buffer_size: u64) -> wgpu::Limits {
        wgpu::Limits {
            max_texture_dimension_2d,
            max_buffer_size,
            ..wgpu::Limits::default()
        }
    }

    #[test]
    fn target_layout_accounts_for_rgba_and_padded_staging_bytes() {
        let layout = TargetLayout::validate(
            PhysicalSize {
                width: 3,
                height: 2,
            },
            536,
            &limits(64, 512),
        )
        .unwrap();
        assert_eq!(layout.unpadded_bytes_per_row, 12);
        assert_eq!(layout.padded_bytes_per_row, 256);
        assert_eq!(layout.rgba_bytes, 24);
        assert_eq!(layout.staging_bytes, 512);
    }

    #[test]
    fn target_layout_rejects_budget_device_and_dimension_limits_before_allocation() {
        let padded_budget = TargetLayout::validate(
            PhysicalSize {
                width: 3,
                height: 2,
            },
            535,
            &limits(64, 512),
        )
        .unwrap_err();
        assert!(padded_budget.to_string().contains("max_target_bytes"));

        let device_buffer = TargetLayout::validate(
            PhysicalSize {
                width: 3,
                height: 2,
            },
            536,
            &limits(64, 511),
        )
        .unwrap_err();
        assert!(device_buffer.to_string().contains("max_buffer_size"));

        let dimension = TargetLayout::validate(
            PhysicalSize {
                width: 65,
                height: 1,
            },
            1024,
            &limits(64, 1024),
        )
        .unwrap_err();
        assert!(dimension.to_string().contains("max_texture_dimension_2d"));
    }
}
