//! Reusable instanced rendering for simple vector primitives.
//!
//! The inputs in this module are intentionally crate-private. They form the
//! GPU-side contract until the backend-neutral scene types are expanded.

use anyhow::Context as _;

use super::patterns::{
    DiagonalPattern, DotPattern, PatternPaint as GpuPatternPaint, RepeatedTexturePattern,
};

const SHADER: &str = include_str!("shaders/primitives.wgsl");
const MAX_GRADIENT_STOPS: usize = 8;
const INSTANCE_SIZE: usize = 256;
const VIEWPORT_SIZE: u64 = 16;
const INITIAL_INSTANCE_CAPACITY: u64 = INSTANCE_SIZE as u64;
const PAINT_KIND_STRIDE: f32 = 8.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct PrimitiveRect {
    pub(crate) x: f32,
    pub(crate) y: f32,
    pub(crate) width: f32,
    pub(crate) height: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum PrimitiveShape {
    Rect,
    RoundedRect {
        /// Top-left, top-right, bottom-right, and bottom-left radii.
        radii: [f32; 4],
    },
    Circle,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum PrimitiveStyle {
    Fill,
    /// A centered stroke, in logical pixels.
    Stroke {
        width: f32,
    },
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct PrimitiveColor(pub(crate) [f32; 4]);

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct GradientStop {
    pub(crate) offset: f32,
    pub(crate) color: PrimitiveColor,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum PrimitivePaint {
    Solid(PrimitiveColor),
    LinearGradient {
        start: [f32; 2],
        end: [f32; 2],
        stops: Vec<GradientStop>,
    },
    RadialGradient {
        center: [f32; 2],
        radii: [f32; 2],
        stops: Vec<GradientStop>,
    },
    Dots {
        pattern: DotPattern,
        foreground: PrimitiveColor,
        background: PrimitiveColor,
    },
    Diagonal {
        pattern: DiagonalPattern,
        foreground: PrimitiveColor,
        background: PrimitiveColor,
    },
    RepeatedTexture {
        pattern: RepeatedTexturePattern,
        tint: PrimitiveColor,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Primitive {
    pub(crate) bounds: PrimitiveRect,
    pub(crate) shape: PrimitiveShape,
    pub(crate) style: PrimitiveStyle,
    pub(crate) paint: PrimitivePaint,
}

/// A render pipeline and upload buffers that are retained across frames.
pub(crate) struct PrimitivePipeline {
    pipeline: wgpu::RenderPipeline,
    bind_group_layout: wgpu::BindGroupLayout,
    viewport_buffer: wgpu::Buffer,
    instance_buffer: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    repeat_texture: wgpu::Texture,
    repeat_sampler: wgpu::Sampler,
    instance_capacity: u64,
    instance_count: u32,
}

impl PrimitivePipeline {
    pub(crate) fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        target_format: wgpu::TextureFormat,
    ) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("rustverse_svg primitive shader"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("rustverse_svg primitive bind group layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: wgpu::BufferSize::new(VIEWPORT_SIZE),
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: true },
                        has_dynamic_offset: false,
                        min_binding_size: wgpu::BufferSize::new(INSTANCE_SIZE as u64),
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("rustverse_svg primitive pipeline layout"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("rustverse_svg primitive pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[],
            },
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                cull_mode: None,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: target_format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });
        let viewport_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("rustverse_svg primitive viewport"),
            size: VIEWPORT_SIZE,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let instance_buffer = create_instance_buffer(device, INITIAL_INSTANCE_CAPACITY);
        let repeat_texture = create_repeat_texture(device, queue);
        let repeat_view = repeat_texture.create_view(&wgpu::TextureViewDescriptor::default());
        let repeat_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("rustverse_svg primitive repeat sampler"),
            address_mode_u: wgpu::AddressMode::Repeat,
            address_mode_v: wgpu::AddressMode::Repeat,
            address_mode_w: wgpu::AddressMode::Repeat,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Linear,
            ..Default::default()
        });
        let bind_group = create_bind_group(
            device,
            &bind_group_layout,
            &viewport_buffer,
            &instance_buffer,
            &repeat_view,
            &repeat_sampler,
        );

        Self {
            pipeline,
            bind_group_layout,
            viewport_buffer,
            instance_buffer,
            bind_group,
            repeat_texture,
            repeat_sampler,
            instance_capacity: INITIAL_INSTANCE_CAPACITY,
            instance_count: 0,
        }
    }

    /// Validates and uploads a batch while retaining allocations when they fit.
    pub(crate) fn prepare(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        viewport: [f32; 2],
        primitives: &[Primitive],
    ) -> anyhow::Result<()> {
        validate_viewport(viewport)?;
        let instance_count =
            u32::try_from(primitives.len()).context("primitive batch exceeds u32::MAX")?;
        let bytes = pack_instances(primitives)?;
        let required = u64::try_from(bytes.len()).context("primitive upload is too large")?;

        let max_binding_size = u64::from(device.limits().max_storage_buffer_binding_size);
        if required > max_binding_size {
            anyhow::bail!(
                "primitive upload requires {required} bytes, exceeding max_storage_buffer_binding_size {}",
                device.limits().max_storage_buffer_binding_size
            );
        }
        if required > self.instance_capacity {
            let capacity = required
                .checked_next_power_of_two()
                .context("primitive buffer capacity overflowed")?
                .min(max_binding_size);
            self.instance_buffer = create_instance_buffer(device, capacity);
            self.bind_group = create_bind_group(
                device,
                &self.bind_group_layout,
                &self.viewport_buffer,
                &self.instance_buffer,
                &self
                    .repeat_texture
                    .create_view(&wgpu::TextureViewDescriptor::default()),
                &self.repeat_sampler,
            );
            self.instance_capacity = capacity;
        }

        let mut viewport_bytes = Vec::with_capacity(VIEWPORT_SIZE as usize);
        push_f32s(&mut viewport_bytes, &[viewport[0], viewport[1], 0.0, 0.0]);
        queue.write_buffer(&self.viewport_buffer, 0, &viewport_bytes);
        if !bytes.is_empty() {
            queue.write_buffer(&self.instance_buffer, 0, &bytes);
        }
        self.instance_count = instance_count;
        Ok(())
    }

    /// Records the prepared batch into an existing color render pass.
    pub(crate) fn draw<'pass>(&'pass self, pass: &mut wgpu::RenderPass<'pass>) {
        if self.instance_count == 0 {
            return;
        }
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &self.bind_group, &[]);
        pass.draw(0..6, 0..self.instance_count);
    }
}

fn create_instance_buffer(device: &wgpu::Device, size: u64) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("rustverse_svg primitive instances"),
        size,
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

fn create_bind_group(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    viewport_buffer: &wgpu::Buffer,
    instance_buffer: &wgpu::Buffer,
    repeat_view: &wgpu::TextureView,
    repeat_sampler: &wgpu::Sampler,
) -> wgpu::BindGroup {
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("rustverse_svg primitive bind group"),
        layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: viewport_buffer.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: instance_buffer.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: wgpu::BindingResource::TextureView(repeat_view),
            },
            wgpu::BindGroupEntry {
                binding: 3,
                resource: wgpu::BindingResource::Sampler(repeat_sampler),
            },
        ],
    })
}

fn create_repeat_texture(device: &wgpu::Device, queue: &wgpu::Queue) -> wgpu::Texture {
    let size = wgpu::Extent3d {
        width: 2,
        height: 2,
        depth_or_array_layers: 1,
    };
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("rustverse_svg resident repeat fixture"),
        size,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        &[
            255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255,
        ],
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(8),
            rows_per_image: Some(2),
        },
        size,
    );
    texture
}

fn validate_viewport(viewport: [f32; 2]) -> anyhow::Result<()> {
    anyhow::ensure!(
        viewport
            .iter()
            .all(|dimension| dimension.is_finite() && *dimension > 0.0),
        "primitive viewport dimensions must be finite and greater than zero"
    );
    Ok(())
}

fn pack_instances(primitives: &[Primitive]) -> anyhow::Result<Vec<u8>> {
    let capacity = primitives
        .len()
        .checked_mul(INSTANCE_SIZE)
        .context("primitive upload byte count overflowed")?;
    let mut bytes = Vec::with_capacity(capacity);
    for primitive in primitives {
        pack_instance(&mut bytes, primitive)?;
    }
    debug_assert_eq!(bytes.len(), capacity);
    Ok(bytes)
}

fn pack_instance(bytes: &mut Vec<u8>, primitive: &Primitive) -> anyhow::Result<()> {
    validate_bounds(primitive.bounds)?;
    let (shape_kind, radii) = match primitive.shape {
        PrimitiveShape::Rect => (0.0, [0.0; 4]),
        PrimitiveShape::RoundedRect { radii } => {
            let max_radius = 0.5 * primitive.bounds.width.min(primitive.bounds.height);
            anyhow::ensure!(
                radii
                    .iter()
                    .all(|radius| radius.is_finite() && *radius >= 0.0 && *radius <= max_radius),
                "rounded rectangle radii must be finite and within 0..={max_radius}"
            );
            (0.0, radii)
        }
        PrimitiveShape::Circle => (1.0, [0.0; 4]),
    };
    let (style_kind, stroke_width) = match primitive.style {
        PrimitiveStyle::Fill => (0.0, 0.0),
        PrimitiveStyle::Stroke { width } => {
            anyhow::ensure!(
                width.is_finite() && width > 0.0,
                "primitive stroke width must be finite and greater than zero"
            );
            (1.0, width)
        }
    };
    let packed_paint = validate_paint(&primitive.paint)?;

    push_f32s(
        bytes,
        &[
            primitive.bounds.x,
            primitive.bounds.y,
            primitive.bounds.width,
            primitive.bounds.height,
        ],
    );
    push_f32s(bytes, &radii);
    push_f32s(
        bytes,
        &[
            stroke_width,
            shape_kind,
            style_kind,
            packed_paint.kind + packed_paint.stop_count as f32 * PAINT_KIND_STRIDE,
        ],
    );
    push_f32s(bytes, &packed_paint.geometry);
    for index in 0..MAX_GRADIENT_STOPS {
        push_f32s(bytes, &packed_paint.colors[index]);
    }
    push_f32s(bytes, &packed_paint.offsets[0..4]);
    push_f32s(bytes, &packed_paint.offsets[4..8]);
    push_f32s(bytes, &packed_paint.transform[0]);
    push_f32s(bytes, &packed_paint.transform[1]);
    Ok(())
}

fn validate_bounds(bounds: PrimitiveRect) -> anyhow::Result<()> {
    anyhow::ensure!(
        [bounds.x, bounds.y, bounds.width, bounds.height]
            .iter()
            .all(|value| value.is_finite()),
        "primitive bounds must be finite"
    );
    anyhow::ensure!(
        bounds.width > 0.0 && bounds.height > 0.0,
        "primitive bounds must have positive width and height"
    );
    Ok(())
}

struct PackedPaint {
    kind: f32,
    stop_count: usize,
    geometry: [f32; 4],
    colors: [[f32; 4]; MAX_GRADIENT_STOPS],
    offsets: [f32; MAX_GRADIENT_STOPS],
    transform: [[f32; 4]; 2],
}

fn validate_paint(paint: &PrimitivePaint) -> anyhow::Result<PackedPaint> {
    let mut packed = PackedPaint {
        kind: 0.0,
        stop_count: 1,
        geometry: [0.0; 4],
        colors: [[0.0; 4]; MAX_GRADIENT_STOPS],
        offsets: [0.0; MAX_GRADIENT_STOPS],
        transform: [[1.0, 0.0, 0.0, 0.0], [0.0, 1.0, 0.0, 0.0]],
    };
    match paint {
        PrimitivePaint::Solid(color) => {
            packed.colors[0] = packed_color(*color)?;
            packed.offsets[0] = 1.0;
        }
        PrimitivePaint::LinearGradient { start, end, stops } => {
            validate_point(*start, "linear gradient start")?;
            validate_point(*end, "linear gradient end")?;
            anyhow::ensure!(
                start != end,
                "linear gradient start and end must be distinct"
            );
            pack_stops(stops, &mut packed)?;
            packed.kind = 1.0;
            packed.geometry = [start[0], start[1], end[0], end[1]];
        }
        PrimitivePaint::RadialGradient {
            center,
            radii,
            stops,
        } => {
            validate_point(*center, "radial gradient center")?;
            anyhow::ensure!(
                radii
                    .iter()
                    .all(|radius| radius.is_finite() && *radius > 0.0),
                "radial gradient radii must be finite and greater than zero"
            );
            pack_stops(stops, &mut packed)?;
            packed.kind = 2.0;
            packed.geometry = [center[0], center[1], radii[0], radii[1]];
        }
        PrimitivePaint::Dots {
            pattern,
            foreground,
            background,
        } => {
            GpuPatternPaint::Dots(*pattern)
                .validate()
                .map_err(|error| anyhow::anyhow!("invalid dot pattern: {error:?}"))?;
            packed.kind = 3.0;
            packed.colors[0] = packed_color(*foreground)?;
            packed.colors[1] = packed_color(*background)?;
            packed.geometry = [
                pattern.tile_size[0],
                pattern.tile_size[1],
                pattern.radius,
                0.0,
            ];
        }
        PrimitivePaint::Diagonal {
            pattern,
            foreground,
            background,
        } => {
            GpuPatternPaint::Diagonal(*pattern)
                .validate()
                .map_err(|error| anyhow::anyhow!("invalid diagonal pattern: {error:?}"))?;
            packed.kind = 4.0;
            packed.colors[0] = packed_color(*foreground)?;
            packed.colors[1] = packed_color(*background)?;
            packed.geometry = [
                pattern.tile_size,
                pattern.tile_size,
                pattern.line_width,
                0.0,
            ];
        }
        PrimitivePaint::RepeatedTexture { pattern, tint } => {
            GpuPatternPaint::RepeatedTexture(*pattern)
                .validate()
                .map_err(|error| anyhow::anyhow!("invalid repeated texture: {error:?}"))?;
            anyhow::ensure!(
                pattern.texture.0 == 1,
                "resident repeat fixture only supports texture handle 1"
            );
            packed.kind = 5.0;
            packed.colors[0] = packed_color(*tint)?;
            packed.geometry = [pattern.tile_size[0], pattern.tile_size[1], 0.0, 0.0];
            packed.transform = [
                [
                    pattern.logical_to_pattern.rows[0][0],
                    pattern.logical_to_pattern.rows[0][1],
                    pattern.logical_to_pattern.rows[0][2],
                    0.0,
                ],
                [
                    pattern.logical_to_pattern.rows[1][0],
                    pattern.logical_to_pattern.rows[1][1],
                    pattern.logical_to_pattern.rows[1][2],
                    0.0,
                ],
            ];
        }
    }
    Ok(packed)
}

fn pack_stops(stops: &[GradientStop], packed: &mut PackedPaint) -> anyhow::Result<()> {
    anyhow::ensure!(
        (2..=MAX_GRADIENT_STOPS).contains(&stops.len()),
        "gradients require 2..={MAX_GRADIENT_STOPS} stops"
    );
    let mut previous = None;
    for (index, stop) in stops.iter().enumerate() {
        anyhow::ensure!(
            stop.offset.is_finite() && (0.0..=1.0).contains(&stop.offset),
            "gradient stop offsets must be finite and within 0..=1"
        );
        if let Some(previous) = previous {
            anyhow::ensure!(
                stop.offset >= previous,
                "gradient stop offsets must be nondecreasing"
            );
        }
        packed.colors[index] = packed_color(stop.color)?;
        packed.offsets[index] = stop.offset;
        previous = Some(stop.offset);
    }
    packed.stop_count = stops.len();
    Ok(())
}

fn validate_point(point: [f32; 2], name: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        point.iter().all(|coordinate| coordinate.is_finite()),
        "{name} must be finite"
    );
    Ok(())
}

fn validate_color(color: PrimitiveColor) -> anyhow::Result<()> {
    anyhow::ensure!(
        color
            .0
            .iter()
            .all(|channel| channel.is_finite() && (0.0..=1.0).contains(channel)),
        "primitive color channels must be finite and within 0..=1"
    );
    Ok(())
}

/// Matches the byte-backed color and opacity representation used by resvg.
///
/// The render target is `Rgba8Unorm`, so quantizing authored channels before
/// interpolation also makes source-over at 50% use 128/255 rather than an
/// unrepresentable exact 0.5.
fn packed_color(color: PrimitiveColor) -> anyhow::Result<[f32; 4]> {
    validate_color(color)?;
    Ok(color.0.map(|channel| (channel * 255.0).round() / 255.0))
}

fn push_f32s(bytes: &mut Vec<u8>, values: &[f32]) {
    for value in values {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stops() -> Vec<GradientStop> {
        vec![
            GradientStop {
                offset: 0.0,
                color: PrimitiveColor([1.0, 0.0, 0.0, 1.0]),
            },
            GradientStop {
                offset: 1.0,
                color: PrimitiveColor([0.0, 0.0, 1.0, 1.0]),
            },
        ]
    }

    fn primitive(paint: PrimitivePaint) -> Primitive {
        Primitive {
            bounds: PrimitiveRect {
                x: 10.0,
                y: 20.0,
                width: 100.0,
                height: 50.0,
            },
            shape: PrimitiveShape::RoundedRect {
                radii: [2.0, 4.0, 6.0, 8.0],
            },
            style: PrimitiveStyle::Stroke { width: 3.0 },
            paint,
        }
    }

    #[test]
    fn packing_matches_shader_stride_and_little_endian_fields() {
        let bytes = pack_instances(&[primitive(PrimitivePaint::LinearGradient {
            start: [10.0, 20.0],
            end: [110.0, 20.0],
            stops: stops(),
        })])
        .unwrap();

        assert_eq!(bytes.len(), INSTANCE_SIZE);
        assert_eq!(&bytes[0..4], &10.0_f32.to_le_bytes());
        assert_eq!(&bytes[16..20], &2.0_f32.to_le_bytes());
        assert_eq!(&bytes[32..36], &3.0_f32.to_le_bytes());
        assert_eq!(&bytes[44..48], &17.0_f32.to_le_bytes());
        assert_eq!(&bytes[48..52], &10.0_f32.to_le_bytes());
        assert_eq!(&bytes[64..68], &1.0_f32.to_le_bytes());
    }

    #[test]
    fn validation_rejects_invalid_geometry_and_gradient_stops() {
        let mut invalid_bounds = primitive(PrimitivePaint::Solid(PrimitiveColor([0.0; 4])));
        invalid_bounds.bounds.width = 0.0;
        assert!(
            pack_instances(&[invalid_bounds])
                .unwrap_err()
                .to_string()
                .contains("positive")
        );

        let invalid_stops = primitive(PrimitivePaint::RadialGradient {
            center: [0.0, 0.0],
            radii: [10.0, 10.0],
            stops: vec![
                GradientStop {
                    offset: 0.6,
                    color: PrimitiveColor([0.0; 4]),
                },
                GradientStop {
                    offset: 0.5,
                    color: PrimitiveColor([1.0; 4]),
                },
            ],
        });
        assert!(
            pack_instances(&[invalid_stops])
                .unwrap_err()
                .to_string()
                .contains("nondecreasing")
        );
    }
}
