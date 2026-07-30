use std::fmt;

use anyhow::Context as _;

use crate::{RenderScale, scene::LogicalSize};

use super::primitives::{Primitive, PrimitivePipeline};

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
pub(crate) struct GpuContext {
    _instance: wgpu::Instance,
    device: wgpu::Device,
    queue: wgpu::Queue,
    primitives: PrimitivePipeline,
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

        let primitives = PrimitivePipeline::new(&device, &queue, RGBA8_TARGET_FORMAT);
        Ok(Self {
            _instance: instance,
            device,
            queue,
            primitives,
        })
    }

    /// Draws a primitive batch over a clear color and returns straight-alpha
    /// readback bytes plus a PNG.
    pub(crate) async fn render_primitives(
        &mut self,
        logical_size: LogicalSize,
        scale: RenderScale,
        color: [f64; 4],
        max_target_bytes: u64,
        primitives: &[Primitive],
    ) -> anyhow::Result<HeadlessImage> {
        anyhow::ensure!(
            color
                .iter()
                .all(|channel| channel.is_finite() && (0.0..=1.0).contains(channel)),
            "clear color channels must be finite and within 0..=1"
        );
        let size = physical_size(logical_size, scale)?;
        let layout = TargetLayout::validate(size, max_target_bytes, &self.device.limits())?;
        let clear_alpha = color[3];
        self.primitives.prepare(
            &self.device,
            &self.queue,
            [logical_size.width, logical_size.height],
            primitives,
        )?;
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
                        r: color[0] * clear_alpha,
                        g: color[1] * clear_alpha,
                        b: color[2] * clear_alpha,
                        a: clear_alpha,
                    }),
                    store: wgpu::StoreOp::Store,
                },
            })];
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("rustverse_svg headless clear pass"),
                color_attachments: &color_attachments,
                ..Default::default()
            });
            self.primitives.draw(&mut pass);
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

        unpremultiply_rgba(&mut rgba);
        let png = encode_png(size, &rgba)?;
        Ok(HeadlessImage { size, rgba, png })
    }
}

fn unpremultiply_rgba(rgba: &mut [u8]) {
    for pixel in rgba.chunks_exact_mut(4) {
        let alpha = u32::from(pixel[3]);
        if alpha == 0 {
            pixel[..3].fill(0);
        } else if alpha < 255 {
            for channel in &mut pixel[..3] {
                let straight = (u32::from(*channel) * 255 + alpha / 2) / alpha;
                *channel = straight.min(255) as u8;
            }
        }
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
    use crate::gpu::{
        patterns::{
            DiagonalPattern, DotPattern, LogicalToPattern, PatternTextureHandle,
            RepeatedTexturePattern,
        },
        primitives::{
            GradientStop, Primitive, PrimitiveColor, PrimitivePaint, PrimitiveRect, PrimitiveShape,
            PrimitiveStyle,
        },
    };

    fn limits(max_texture_dimension_2d: u32, max_buffer_size: u64) -> wgpu::Limits {
        wgpu::Limits {
            max_texture_dimension_2d,
            max_buffer_size,
            ..wgpu::Limits::default()
        }
    }

    async fn context_or_skip() -> Option<GpuContext> {
        match GpuContext::new().await {
            Ok(context) => Some(context),
            Err(GpuInitError::AdapterUnavailable(error)) => {
                if std::env::var_os("RUSTVERSE_REQUIRE_GPU").is_some_and(|value| value == "1") {
                    panic!(
                        "RUSTVERSE_REQUIRE_GPU=1 but no surface-free wgpu adapter is available: \
                         {error}"
                    );
                }
                eprintln!("SKIP: no surface-free wgpu adapter is available: {error}");
                None
            }
            Err(error) => panic!("GPU context initialization failed: {error}"),
        }
    }

    fn color(red: f32, green: f32, blue: f32, alpha: f32) -> PrimitiveColor {
        PrimitiveColor([red, green, blue, alpha])
    }

    fn solid(
        bounds: PrimitiveRect,
        shape: PrimitiveShape,
        style: PrimitiveStyle,
        value: PrimitiveColor,
    ) -> Primitive {
        Primitive {
            bounds,
            shape,
            style,
            paint: PrimitivePaint::Solid(value),
        }
    }

    fn pixel(image: &HeadlessImage, x: u32, y: u32) -> [u8; 4] {
        let offset = ((y * image.size.width + x) * 4) as usize;
        image.rgba[offset..offset + 4].try_into().unwrap()
    }

    async fn render(
        context: &mut GpuContext,
        logical_size: LogicalSize,
        scale: f32,
        clear: [f64; 4],
        primitives: &[Primitive],
    ) -> HeadlessImage {
        context
            .render_primitives(
                logical_size,
                RenderScale::new(scale).unwrap(),
                clear,
                1024 * 1024,
                primitives,
            )
            .await
            .unwrap()
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

    #[tokio::test]
    async fn gpu_readback_covers_shapes_styles_and_gradients() {
        let Some(mut context) = context_or_skip().await else {
            return;
        };
        let gradient_stops = vec![
            GradientStop {
                offset: 0.0,
                color: color(1.0, 0.0, 0.0, 1.0),
            },
            GradientStop {
                offset: 1.0,
                color: color(0.0, 0.0, 1.0, 1.0),
            },
        ];
        let primitives = vec![
            solid(
                PrimitiveRect {
                    x: 1.0,
                    y: 1.0,
                    width: 4.0,
                    height: 4.0,
                },
                PrimitiveShape::Rect,
                PrimitiveStyle::Fill,
                color(1.0, 0.0, 0.0, 1.0),
            ),
            solid(
                PrimitiveRect {
                    x: 6.0,
                    y: 1.0,
                    width: 5.0,
                    height: 5.0,
                },
                PrimitiveShape::RoundedRect { radii: [2.0; 4] },
                PrimitiveStyle::Fill,
                color(0.0, 1.0, 0.0, 1.0),
            ),
            solid(
                PrimitiveRect {
                    x: 12.0,
                    y: 1.0,
                    width: 5.0,
                    height: 5.0,
                },
                PrimitiveShape::Circle,
                PrimitiveStyle::Fill,
                color(0.0, 0.0, 1.0, 1.0),
            ),
            solid(
                PrimitiveRect {
                    x: 18.0,
                    y: 1.0,
                    width: 5.0,
                    height: 5.0,
                },
                PrimitiveShape::Rect,
                PrimitiveStyle::Stroke { width: 2.0 },
                color(1.0, 1.0, 0.0, 1.0),
            ),
            Primitive {
                bounds: PrimitiveRect {
                    x: 1.0,
                    y: 8.0,
                    width: 8.0,
                    height: 4.0,
                },
                shape: PrimitiveShape::Rect,
                style: PrimitiveStyle::Fill,
                paint: PrimitivePaint::LinearGradient {
                    start: [1.0, 8.0],
                    end: [9.0, 8.0],
                    stops: gradient_stops.clone(),
                },
            },
            Primitive {
                bounds: PrimitiveRect {
                    x: 11.0,
                    y: 8.0,
                    width: 8.0,
                    height: 6.0,
                },
                shape: PrimitiveShape::Rect,
                style: PrimitiveStyle::Fill,
                paint: PrimitivePaint::RadialGradient {
                    center: [15.0, 11.0],
                    radii: [4.0, 4.0],
                    stops: gradient_stops,
                },
            },
        ];
        let image = render(
            &mut context,
            LogicalSize {
                width: 24.0,
                height: 16.0,
            },
            1.0,
            [0.0, 0.0, 0.0, 1.0],
            &primitives,
        )
        .await;

        assert_eq!(pixel(&image, 2, 2), [255, 0, 0, 255]);
        assert_eq!(pixel(&image, 8, 3), [0, 255, 0, 255]);
        assert_eq!(pixel(&image, 14, 3), [0, 0, 255, 255]);
        assert_eq!(pixel(&image, 18, 3), [255, 255, 0, 255]);
        assert_eq!(pixel(&image, 17, 3), [255, 255, 0, 255]);
        assert_eq!(pixel(&image, 20, 3), [0, 0, 0, 255]);
        let linear_start = pixel(&image, 2, 10);
        let linear_end = pixel(&image, 7, 10);
        assert!(linear_start[0] > linear_start[2]);
        assert!(linear_end[2] > linear_end[0]);
        let radial_center = pixel(&image, 14, 10);
        let radial_edge = pixel(&image, 18, 10);
        assert!(radial_center[0] > radial_center[2]);
        assert!(radial_edge[2] > radial_edge[0]);
    }

    #[tokio::test]
    async fn rgba8_unorm_gradient_midpoints_match_the_reference_bytes() {
        let Some(mut context) = context_or_skip().await else {
            return;
        };
        let midpoint = |x, left, right| Primitive {
            bounds: PrimitiveRect {
                x,
                y: 0.0,
                width: 5.0,
                height: 5.0,
            },
            shape: PrimitiveShape::Rect,
            style: PrimitiveStyle::Fill,
            paint: PrimitivePaint::LinearGradient {
                start: [x + 1.0, 0.0],
                end: [x + 4.0, 0.0],
                stops: vec![
                    GradientStop {
                        offset: 0.0,
                        color: left,
                    },
                    GradientStop {
                        offset: 1.0,
                        color: right,
                    },
                ],
            },
        };
        let image = render(
            &mut context,
            LogicalSize {
                width: 10.0,
                height: 5.0,
            },
            1.0,
            [0.0, 0.0, 0.0, 1.0],
            &[
                midpoint(0.0, color(0.0, 0.0, 0.0, 1.0), color(1.0, 1.0, 1.0, 1.0)),
                midpoint(5.0, color(1.0, 0.0, 0.0, 1.0), color(0.0, 0.0, 1.0, 1.0)),
            ],
        )
        .await;

        assert_eq!(pixel(&image, 2, 2), [128, 128, 128, 255]);
        assert_eq!(pixel(&image, 7, 2), [128, 0, 128, 255]);
    }

    #[tokio::test]
    async fn logical_geometry_scales_with_the_physical_target_and_reuses_pipeline() {
        let Some(mut context) = context_or_skip().await else {
            return;
        };
        let primitive = solid(
            PrimitiveRect {
                x: 2.0,
                y: 2.0,
                width: 4.0,
                height: 3.0,
            },
            PrimitiveShape::Rect,
            PrimitiveStyle::Fill,
            color(1.0, 0.0, 0.0, 1.0),
        );
        let logical_size = LogicalSize {
            width: 8.0,
            height: 6.0,
        };
        for (scale, expected_size) in [
            (0.5, (4, 3)),
            (1.0, (8, 6)),
            (1.25, (10, 8)),
            (2.0, (16, 12)),
            (5.0, (40, 30)),
        ] {
            let image = render(
                &mut context,
                logical_size,
                scale,
                [0.0, 0.0, 0.0, 1.0],
                std::slice::from_ref(&primitive),
            )
            .await;
            assert_eq!(
                (image.size.width, image.size.height),
                expected_size,
                "wrong physical size at scale {scale}"
            );
            let inside_x = (4.0 * scale).floor() as u32;
            let inside_y = (3.5 * scale).floor() as u32;
            let outside = scale.floor() as u32;
            let inside_pixel = pixel(&image, inside_x, inside_y);
            assert!(
                inside_pixel[0] >= 200
                    && inside_pixel[1] == 0
                    && inside_pixel[2] == 0
                    && inside_pixel[3] == 255,
                "logical fill moved at scale {scale}: {inside_pixel:?}"
            );
            let outside_pixel = pixel(&image, outside, inside_y);
            assert!(
                outside_pixel[0] <= 16
                    && outside_pixel[1] == 0
                    && outside_pixel[2] == 0
                    && outside_pixel[3] == 255,
                "logical exterior moved at scale {scale}: {outside_pixel:?}"
            );
        }
    }

    #[tokio::test]
    async fn procedural_patterns_match_svg_lattices_and_diagonal_phase_at_scale_one() {
        let Some(mut context) = context_or_skip().await else {
            return;
        };
        let full = |x, paint| Primitive {
            bounds: PrimitiveRect {
                x,
                y: 0.0,
                width: 8.0,
                height: 8.0,
            },
            shape: PrimitiveShape::Rect,
            style: PrimitiveStyle::Fill,
            paint,
        };
        let primitives = [
            full(
                0.0,
                PrimitivePaint::Dots {
                    pattern: DotPattern {
                        tile_size: [8.0, 8.0],
                        radius: 1.5,
                    },
                    foreground: color(1.0, 1.0, 1.0, 1.0),
                    background: color(1.0, 0.0, 0.0, 1.0),
                },
            ),
            full(
                8.0,
                PrimitivePaint::Diagonal {
                    pattern: DiagonalPattern {
                        tile_size: 4.0,
                        line_width: 2.0,
                    },
                    foreground: color(1.0, 1.0, 1.0, 1.0),
                    background: color(0.0, 0.0, 0.0, 1.0),
                },
            ),
        ];
        let image = render(
            &mut context,
            LogicalSize {
                width: 16.0,
                height: 8.0,
            },
            1.0,
            [0.0, 0.0, 0.0, 1.0],
            &primitives,
        )
        .await;

        // The dot tile has both the shared corner lattice and the center
        // lattice, with empty space between them.
        let corner_dot = pixel(&image, 0, 0);
        let center_dot = pixel(&image, 4, 4);
        assert!(
            corner_dot[1] >= 250 && corner_dot[2] >= 250,
            "{corner_dot:?}"
        );
        assert!(
            center_dot[1] >= 250 && center_dot[2] >= 250,
            "{center_dot:?}"
        );
        assert_eq!(pixel(&image, 0, 4), [255, 0, 0, 255]);
        assert_eq!(pixel(&image, 4, 0), [255, 0, 0, 255]);

        // Pixel centers make x+y equal to an integer. The SVG stripe occupies
        // mod(x+y, 4) in [2, 4], with half coverage at each periodic edge.
        assert_eq!(pixel(&image, 8, 0), [0, 0, 0, 255]);
        assert_eq!(pixel(&image, 10, 0), [255, 255, 255, 255]);
        assert_eq!(pixel(&image, 9, 0), [127, 127, 127, 255]);
        assert_eq!(pixel(&image, 11, 0), [128, 128, 128, 255]);
    }

    #[tokio::test]
    async fn repeated_texture_applies_minus_45_degree_transform_with_linear_repeat() {
        let Some(mut context) = context_or_skip().await else {
            return;
        };
        let inverse_sqrt_two = std::f32::consts::FRAC_1_SQRT_2;
        let primitive = Primitive {
            bounds: PrimitiveRect {
                x: 0.0,
                y: 0.0,
                width: 8.0,
                height: 8.0,
            },
            shape: PrimitiveShape::Rect,
            style: PrimitiveStyle::Fill,
            paint: PrimitivePaint::RepeatedTexture {
                pattern: RepeatedTexturePattern {
                    texture: PatternTextureHandle(1),
                    tile_size: [2.0, 2.0],
                    logical_to_pattern: LogicalToPattern {
                        rows: [
                            [inverse_sqrt_two, inverse_sqrt_two, 0.0],
                            [-inverse_sqrt_two, inverse_sqrt_two, 0.0],
                        ],
                    },
                },
                tint: color(1.0, 1.0, 1.0, 1.0),
            },
        };
        let image = render(
            &mut context,
            LogicalSize {
                width: 8.0,
                height: 8.0,
            },
            1.0,
            [0.0, 0.0, 0.0, 1.0],
            &[primitive],
        )
        .await;

        let diagonal = pixel(&image, 0, 0);
        let horizontal = pixel(&image, 2, 0);
        assert!(
            diagonal[..3].iter().filter(|channel| **channel > 0).count() >= 2,
            "linear sampling after rotation should mix fixture texels: {diagonal:?}"
        );
        assert_ne!(
            diagonal, horizontal,
            "the -45-degree logical-to-pattern rotation must affect sampling"
        );
        let later_diagonal = pixel(&image, 4, 4);
        assert_eq!(diagonal[0], diagonal[2], "{diagonal:?}");
        assert_eq!(later_diagonal[0], later_diagonal[2], "{later_diagonal:?}");
        assert_ne!(
            diagonal[1], later_diagonal[1],
            "linear repeat should advance along the rotated x=y axis"
        );
    }

    #[tokio::test]
    async fn procedural_pattern_edges_stay_in_logical_space_across_scales() {
        let Some(mut context) = context_or_skip().await else {
            return;
        };
        let primitives = [
            Primitive {
                bounds: PrimitiveRect {
                    x: 0.0,
                    y: 0.0,
                    width: 8.0,
                    height: 8.0,
                },
                shape: PrimitiveShape::Rect,
                style: PrimitiveStyle::Fill,
                paint: PrimitivePaint::Dots {
                    pattern: DotPattern {
                        tile_size: [8.0, 8.0],
                        radius: 1.5,
                    },
                    foreground: color(1.0, 1.0, 1.0, 1.0),
                    background: color(1.0, 0.0, 0.0, 1.0),
                },
            },
            Primitive {
                bounds: PrimitiveRect {
                    x: 8.0,
                    y: 0.0,
                    width: 4.0,
                    height: 8.0,
                },
                shape: PrimitiveShape::Rect,
                style: PrimitiveStyle::Fill,
                paint: PrimitivePaint::Diagonal {
                    pattern: DiagonalPattern {
                        tile_size: 4.0,
                        line_width: 2.0,
                    },
                    foreground: color(1.0, 1.0, 1.0, 1.0),
                    background: color(0.0, 0.0, 0.0, 1.0),
                },
            },
        ];
        for scale in [1.0, 1.25, 2.0, 5.0] {
            let image = render(
                &mut context,
                LogicalSize {
                    width: 12.0,
                    height: 8.0,
                },
                scale,
                [0.0, 0.0, 0.0, 1.0],
                &primitives,
            )
            .await;
            let at = |logical_x: f32, logical_y: f32| {
                pixel(
                    &image,
                    (logical_x * scale).floor() as u32,
                    (logical_y * scale).floor() as u32,
                )
            };
            let corner_dot = at(0.0, 0.0);
            let center_dot = at(4.0, 4.0);
            let between_dots = at(0.0, 4.0);
            assert!(
                corner_dot[1] > 200 && center_dot[1] > 200 && between_dots[1] < 32,
                "dot lattice moved at scale {scale}: \
                 corner={corner_dot:?}, center={center_dot:?}, gap={between_dots:?}"
            );
            let stripe_background = at(8.0, 0.0);
            let stripe_foreground = at(10.0, 0.0);
            assert!(
                stripe_background[0] < 64 && stripe_foreground[0] > 192,
                "diagonal stripe phase moved at scale {scale}: \
                 background={stripe_background:?}, foreground={stripe_foreground:?}"
            );
        }
    }

    #[tokio::test]
    async fn shared_background_and_empty_card_shell_render_without_resvg() {
        let Some(mut context) = context_or_skip().await else {
            return;
        };
        let card_bounds = PrimitiveRect {
            x: 2.0,
            y: 2.0,
            width: 12.0,
            height: 8.0,
        };
        let rounded = PrimitiveShape::RoundedRect { radii: [2.0; 4] };
        let primitives = [
            Primitive {
                bounds: PrimitiveRect {
                    x: 0.0,
                    y: 0.0,
                    width: 16.0,
                    height: 12.0,
                },
                shape: PrimitiveShape::Rect,
                style: PrimitiveStyle::Fill,
                paint: PrimitivePaint::Diagonal {
                    pattern: DiagonalPattern::BACKGROUND,
                    foreground: color(0.16, 0.18, 0.22, 1.0),
                    background: color(0.08, 0.09, 0.12, 1.0),
                },
            },
            Primitive {
                bounds: card_bounds,
                shape: rounded,
                style: PrimitiveStyle::Fill,
                paint: PrimitivePaint::LinearGradient {
                    start: [2.0, 2.0],
                    end: [14.0, 10.0],
                    stops: vec![
                        GradientStop {
                            offset: 0.0,
                            color: color(0.12, 0.18, 0.26, 1.0),
                        },
                        GradientStop {
                            offset: 1.0,
                            color: color(0.28, 0.36, 0.46, 1.0),
                        },
                    ],
                },
            },
            solid(
                card_bounds,
                rounded,
                PrimitiveStyle::Stroke { width: 2.0 },
                color(0.85, 0.9, 1.0, 1.0),
            ),
        ];
        let image = render(
            &mut context,
            LogicalSize {
                width: 16.0,
                height: 12.0,
            },
            1.0,
            [0.0, 0.0, 0.0, 1.0],
            &primitives,
        )
        .await;

        let center = pixel(&image, 8, 6);
        assert!(center[0] > 30 && center[2] > center[0]);
        let border = pixel(&image, 2, 6);
        assert!(border[0] >= 180 && border[2] >= border[0]);
        assert_ne!(pixel(&image, 0, 0), center);
    }

    #[tokio::test]
    async fn straight_alpha_readback_unpremultiplies_and_composites_exactly() {
        let Some(mut context) = context_or_skip().await else {
            return;
        };
        let half_red = solid(
            PrimitiveRect {
                x: 0.0,
                y: 0.0,
                width: 4.0,
                height: 4.0,
            },
            PrimitiveShape::Rect,
            PrimitiveStyle::Fill,
            color(1.0, 0.0, 0.0, 0.5),
        );
        let logical_size = LogicalSize {
            width: 4.0,
            height: 4.0,
        };
        let transparent = render(
            &mut context,
            logical_size,
            1.0,
            [0.0, 0.0, 0.0, 0.0],
            std::slice::from_ref(&half_red),
        )
        .await;
        let blue = render(
            &mut context,
            logical_size,
            1.0,
            [0.0, 0.0, 1.0, 1.0],
            &[half_red],
        )
        .await;

        assert_eq!(pixel(&transparent, 2, 2), [255, 0, 0, 128]);
        assert_eq!(pixel(&blue, 2, 2), [128, 0, 127, 255]);
    }

    #[tokio::test]
    async fn translucent_primitives_composite_in_painter_order() {
        let Some(mut context) = context_or_skip().await else {
            return;
        };
        let layer = |value| {
            solid(
                PrimitiveRect {
                    x: 0.0,
                    y: 0.0,
                    width: 4.0,
                    height: 4.0,
                },
                PrimitiveShape::Rect,
                PrimitiveStyle::Fill,
                value,
            )
        };
        let red = layer(color(1.0, 0.0, 0.0, 0.5));
        let green = layer(color(0.0, 1.0, 0.0, 0.5));
        let logical_size = LogicalSize {
            width: 4.0,
            height: 4.0,
        };
        let red_then_green = render(
            &mut context,
            logical_size,
            1.0,
            [0.0, 0.0, 0.0, 0.0],
            &[red.clone(), green.clone()],
        )
        .await;
        let green_then_red = render(
            &mut context,
            logical_size,
            1.0,
            [0.0, 0.0, 0.0, 0.0],
            &[green, red],
        )
        .await;

        let green_on_top = pixel(&red_then_green, 2, 2);
        let red_on_top = pixel(&green_then_red, 2, 2);
        assert_eq!(green_on_top[3], red_on_top[3]);
        assert!(green_on_top[1] > green_on_top[0], "{green_on_top:?}");
        assert!(red_on_top[0] > red_on_top[1], "{red_on_top:?}");
        assert_eq!(
            (green_on_top[0], green_on_top[1]),
            (red_on_top[1], red_on_top[0])
        );
    }

    #[test]
    fn unpremultiply_converts_partial_alpha_and_preserves_opaque_bytes() {
        let mut rgba = [128, 0, 0, 128, 17, 34, 51, 255, 99, 88, 77, 0];
        unpremultiply_rgba(&mut rgba);
        assert_eq!(rgba, [255, 0, 0, 128, 17, 34, 51, 255, 0, 0, 0, 0]);
    }
}
