//! Persistent atlas textures and the image sampling pipeline.

use anyhow::Context as _;

use crate::scene::{ImageDimensions, ImageGeometryError, ImageHandle, ImageNode};

use super::{
    decode::DecodedImage,
    packing::AtlasSetModel,
    types::{AtlasPage, RegionHandle, ResidentRegion},
};

const SHADER: &str = include_str!("../shaders/images.wgsl");
const VIEWPORT_BUFFER_SIZE: u64 = 16;
const IMAGE_INSTANCE_SIZE: usize = 32;
const INITIAL_INSTANCE_CAPACITY: u64 = IMAGE_INSTANCE_SIZE as u64;

/// Texture allocation retained for the complete renderer lifetime.
pub(crate) struct GpuAtlasPage {
    pub(crate) texture: wgpu::Texture,
    pub(crate) view: wgpu::TextureView,
}

impl GpuAtlasPage {
    pub(crate) fn new(device: &wgpu::Device, page: &AtlasPage) -> Self {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("rustverse_svg persistent image atlas page"),
            size: wgpu::Extent3d {
                width: page.width,
                height: page.height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        Self { texture, view }
    }
}

/// One decoded unique image and its already-planned resident region.
pub(crate) struct AtlasUpload<'a> {
    pub(crate) region: &'a ResidentRegion,
    pub(crate) decoded: &'a DecodedImage,
}

/// Upload counters remain stable during warm draw preparation.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct GpuUploadMetrics {
    pub(crate) upload_count: u64,
    pub(crate) upload_bytes: u64,
}

/// Uploads one image plus extruded padding into its persistent page.
pub(crate) fn upload_region(
    queue: &wgpu::Queue,
    page: &GpuAtlasPage,
    page_metadata: &AtlasPage,
    padding: u32,
    upload: AtlasUpload<'_>,
) -> anyhow::Result<u64> {
    let region = upload.region;
    let decoded = upload.decoded;
    anyhow::ensure!(
        decoded.width == region.source_width.get()
            && decoded.height == region.source_height.get()
            && decoded.content_hash == region.content_hash,
        "decoded atlas upload does not match its resident region"
    );
    anyhow::ensure!(
        region.pixels.x >= padding
            && region.pixels.y >= padding
            && region.pixels.right().saturating_add(padding) <= page_metadata.width
            && region.pixels.bottom().saturating_add(padding) <= page_metadata.height,
        "resident atlas padding is outside its page"
    );

    let extruded_width = decoded
        .width
        .checked_add(padding.checked_mul(2).context("atlas padding overflowed")?)
        .context("extruded atlas width overflowed")?;
    let extruded_height = decoded
        .height
        .checked_add(padding.checked_mul(2).context("atlas padding overflowed")?)
        .context("extruded atlas height overflowed")?;
    let byte_len = usize::try_from(
        u64::from(extruded_width)
            .checked_mul(u64::from(extruded_height))
            .and_then(|pixels| pixels.checked_mul(4))
            .context("extruded upload byte count overflowed")?,
    )
    .context("extruded upload exceeds addressable memory")?;
    let mut pixels = vec![0; byte_len];

    for output_y in 0..extruded_height {
        let source_y = output_y.saturating_sub(padding).min(decoded.height - 1);
        for output_x in 0..extruded_width {
            let source_x = output_x.saturating_sub(padding).min(decoded.width - 1);
            let source_offset = usize::try_from(
                (u64::from(source_y) * u64::from(decoded.width) + u64::from(source_x)) * 4,
            )
            .context("source pixel offset overflowed")?;
            let output_offset = usize::try_from(
                (u64::from(output_y) * u64::from(extruded_width) + u64::from(output_x)) * 4,
            )
            .context("output pixel offset overflowed")?;
            pixels[output_offset..output_offset + 4]
                .copy_from_slice(&decoded.rgba8[source_offset..source_offset + 4]);
        }
    }

    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &page.texture,
            mip_level: 0,
            origin: wgpu::Origin3d {
                x: region.pixels.x - padding,
                y: region.pixels.y - padding,
                z: 0,
            },
            aspect: wgpu::TextureAspect::All,
        },
        &pixels,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(extruded_width * 4),
            rows_per_image: Some(extruded_height),
        },
        wgpu::Extent3d {
            width: extruded_width,
            height: extruded_height,
            depth_or_array_layers: 1,
        },
    );
    u64::try_from(pixels.len()).context("upload byte count exceeds u64")
}

/// GPU-ready image quad after handle resolution and source-relative UV fitting.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ImageDraw {
    pub(crate) page: u32,
    pub(crate) destination: [f32; 4],
    pub(crate) atlas_uv: [f32; 4],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PreparedImageDraw {
    pub(crate) page: u32,
    pub(crate) instance: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum PrepareImageError {
    MissingHandle(ImageHandle),
    StaleHandle(ImageHandle),
    SourceDimensionsMismatch {
        handle: ImageHandle,
        expected: (u32, u32),
        actual: (u32, u32),
    },
    PageOutOfBounds(u32),
    Geometry(ImageGeometryError),
}

impl std::fmt::Display for PrepareImageError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingHandle(handle) => write!(formatter, "missing image handle {handle:?}"),
            Self::StaleHandle(handle) => write!(formatter, "stale image handle {handle:?}"),
            Self::SourceDimensionsMismatch {
                handle,
                expected,
                actual,
            } => write!(
                formatter,
                "image handle {handle:?} has source dimensions {}x{}, not {}x{}",
                expected.0, expected.1, actual.0, actual.1
            ),
            Self::PageOutOfBounds(page) => write!(formatter, "atlas page {page} is unavailable"),
            Self::Geometry(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for PrepareImageError {}

impl From<ImageGeometryError> for PrepareImageError {
    fn from(error: ImageGeometryError) -> Self {
        Self::Geometry(error)
    }
}

pub(crate) fn scene_handle(handle: RegionHandle) -> ImageHandle {
    ImageHandle::new(handle.index, handle.generation)
}

pub(crate) fn region_handle(handle: ImageHandle) -> RegionHandle {
    RegionHandle {
        index: handle.slot(),
        generation: handle.generation(),
    }
}

pub(crate) fn resolve_image_draw(
    model: &AtlasSetModel,
    page_count: usize,
    node: &ImageNode,
) -> Result<ImageDraw, PrepareImageError> {
    let handle = region_handle(node.handle());
    let region = match model.region(handle) {
        Some(region) => region,
        None if usize::try_from(handle.index)
            .map_or(true, |index| index >= model.regions().len()) =>
        {
            return Err(PrepareImageError::MissingHandle(node.handle()));
        }
        None => return Err(PrepareImageError::StaleHandle(node.handle())),
    };
    if region.page as usize >= page_count {
        return Err(PrepareImageError::PageOutOfBounds(region.page));
    }
    let expected = (region.source_width.get(), region.source_height.get());
    let actual = (node.source().width(), node.source().height());
    if expected != actual {
        return Err(PrepareImageError::SourceDimensionsMismatch {
            handle: node.handle(),
            expected,
            actual,
        });
    }

    let placement = node.placement()?;
    let destination = placement.destination();
    let source_uv = placement.uv();
    let resident_width = region.uv.max[0] - region.uv.min[0];
    let resident_height = region.uv.max[1] - region.uv.min[1];
    let min_u = region.uv.min[0] + source_uv.x() * resident_width;
    let min_v = region.uv.min[1] + source_uv.y() * resident_height;
    let max_u = min_u + source_uv.width() * resident_width;
    let max_v = min_v + source_uv.height() * resident_height;
    Ok(ImageDraw {
        page: region.page,
        destination: [
            destination.x(),
            destination.y(),
            destination.width(),
            destination.height(),
        ],
        atlas_uv: [min_u, min_v, max_u, max_v],
    })
}

/// Reusable image pipeline. Page bind groups are created once and rebuilt only
/// if the persistent instance buffer must grow.
pub(crate) struct ImagePipeline {
    pipeline: wgpu::RenderPipeline,
    bind_group_layout: wgpu::BindGroupLayout,
    viewport_buffer: wgpu::Buffer,
    instance_buffer: wgpu::Buffer,
    sampler: wgpu::Sampler,
    bind_groups: Vec<wgpu::BindGroup>,
    instance_capacity: u64,
    prepared_count: u32,
}

impl ImagePipeline {
    pub(crate) fn new(device: &wgpu::Device, pages: &[GpuAtlasPage]) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("rustverse_svg atlas image shader"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("rustverse_svg atlas image bind group layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: wgpu::BufferSize::new(VIEWPORT_BUFFER_SIZE),
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: true },
                        has_dynamic_offset: false,
                        min_binding_size: wgpu::BufferSize::new(IMAGE_INSTANCE_SIZE as u64),
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
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("rustverse_svg atlas image pipeline layout"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("rustverse_svg atlas image pipeline"),
            layout: Some(&layout),
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
                    format: wgpu::TextureFormat::Rgba8Unorm,
                    blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });
        let viewport_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("rustverse_svg atlas image viewport"),
            size: VIEWPORT_BUFFER_SIZE,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let instance_buffer = create_instance_buffer(device, INITIAL_INSTANCE_CAPACITY);
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("rustverse_svg atlas clamp linear sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Linear,
            ..Default::default()
        });
        let bind_groups = create_bind_groups(
            device,
            &bind_group_layout,
            &viewport_buffer,
            &instance_buffer,
            &sampler,
            pages,
        );
        Self {
            pipeline,
            bind_group_layout,
            viewport_buffer,
            instance_buffer,
            sampler,
            bind_groups,
            instance_capacity: INITIAL_INSTANCE_CAPACITY,
            prepared_count: 0,
        }
    }

    pub(crate) fn prepare(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        pages: &[GpuAtlasPage],
        viewport: [f32; 2],
        draws: &[ImageDraw],
    ) -> anyhow::Result<Vec<PreparedImageDraw>> {
        anyhow::ensure!(
            viewport
                .iter()
                .all(|dimension| dimension.is_finite() && *dimension > 0.0),
            "image viewport dimensions must be finite and greater than zero"
        );
        let prepared_count =
            u32::try_from(draws.len()).context("image draw count exceeds u32::MAX")?;
        let bytes = pack_instances(draws)?;
        let required = u64::try_from(bytes.len()).context("image upload is too large")?;
        let max_binding_size = u64::from(device.limits().max_storage_buffer_binding_size);
        if required > max_binding_size {
            anyhow::bail!(
                "image instances require {required} bytes, exceeding max_storage_buffer_binding_size {max_binding_size}"
            );
        }
        if required > self.instance_capacity {
            let capacity = required
                .checked_next_power_of_two()
                .context("image instance buffer capacity overflowed")?
                .min(max_binding_size);
            self.instance_buffer = create_instance_buffer(device, capacity);
            self.bind_groups = create_bind_groups(
                device,
                &self.bind_group_layout,
                &self.viewport_buffer,
                &self.instance_buffer,
                &self.sampler,
                pages,
            );
            self.instance_capacity = capacity;
        }

        let mut viewport_bytes = Vec::with_capacity(VIEWPORT_BUFFER_SIZE as usize);
        push_f32s(&mut viewport_bytes, &[viewport[0], viewport[1], 0.0, 0.0]);
        queue.write_buffer(&self.viewport_buffer, 0, &viewport_bytes);
        if !bytes.is_empty() {
            queue.write_buffer(&self.instance_buffer, 0, &bytes);
        }
        self.prepared_count = prepared_count;
        draws
            .iter()
            .enumerate()
            .map(|(index, draw)| {
                Ok(PreparedImageDraw {
                    page: draw.page,
                    instance: u32::try_from(index)
                        .context("image instance index exceeds u32::MAX")?,
                })
            })
            .collect()
    }

    pub(crate) fn sync_pages(&mut self, device: &wgpu::Device, pages: &[GpuAtlasPage]) {
        self.bind_groups = create_bind_groups(
            device,
            &self.bind_group_layout,
            &self.viewport_buffer,
            &self.instance_buffer,
            &self.sampler,
            pages,
        );
    }

    pub(crate) fn draw<'pass>(
        &'pass self,
        pass: &mut wgpu::RenderPass<'pass>,
        prepared: PreparedImageDraw,
    ) {
        debug_assert!(prepared.instance < self.prepared_count);
        let bind_group = self
            .bind_groups
            .get(prepared.page as usize)
            .expect("prepared image draw must reference a resident atlas page");
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, bind_group, &[]);
        pass.draw(0..6, prepared.instance..prepared.instance.saturating_add(1));
    }
}

fn create_instance_buffer(device: &wgpu::Device, size: u64) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("rustverse_svg atlas image instances"),
        size,
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

fn create_bind_groups(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    viewport_buffer: &wgpu::Buffer,
    instance_buffer: &wgpu::Buffer,
    sampler: &wgpu::Sampler,
    pages: &[GpuAtlasPage],
) -> Vec<wgpu::BindGroup> {
    pages
        .iter()
        .map(|page| {
            device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("rustverse_svg atlas page bind group"),
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
                        resource: wgpu::BindingResource::TextureView(&page.view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 3,
                        resource: wgpu::BindingResource::Sampler(sampler),
                    },
                ],
            })
        })
        .collect()
}

fn pack_instances(draws: &[ImageDraw]) -> anyhow::Result<Vec<u8>> {
    let capacity = draws
        .len()
        .checked_mul(IMAGE_INSTANCE_SIZE)
        .context("image instance byte count overflowed")?;
    let mut bytes = Vec::with_capacity(capacity);
    for draw in draws {
        anyhow::ensure!(
            draw.destination
                .iter()
                .chain(draw.atlas_uv.iter())
                .all(|value| value.is_finite()),
            "image draw contains non-finite geometry"
        );
        push_f32s(&mut bytes, &draw.destination);
        push_f32s(&mut bytes, &draw.atlas_uv);
    }
    debug_assert_eq!(bytes.len(), capacity);
    Ok(bytes)
}

fn push_f32s(bytes: &mut Vec<u8>, values: &[f32]) {
    for value in values {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
}

pub(crate) fn dimensions(region: &ResidentRegion) -> ImageDimensions {
    ImageDimensions::new(region.source_width.get(), region.source_height.get())
        .expect("resident atlas dimensions are non-zero")
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;

    use super::super::types::{
        AssetClass, AssetKey, AtlasConfig, ContentHash, DecodedAssetMetadata, ImageFormat,
    };
    use super::*;
    use crate::scene::{ImageFit, Rect};

    fn model() -> AtlasSetModel {
        AtlasSetModel::build_startup(
            AtlasConfig::new(32, 32, 1, 2, 2 * 32 * 32 * 4).unwrap(),
            [DecodedAssetMetadata::new(
                AssetKey::new("fixture").unwrap(),
                ContentHash::new([1; 32]),
                16,
                8,
                ImageFormat::Rgba8Unorm,
                AssetClass::StartupStatic,
            )
            .unwrap()],
        )
        .unwrap()
    }

    #[test]
    fn cover_uv_is_resolved_inside_resident_page_region() {
        let model = model();
        let handle = model.handle(&AssetKey::new("fixture").unwrap()).unwrap();
        let region = model.region(handle).unwrap();
        let node = ImageNode::new(
            scene_handle(handle),
            dimensions(region),
            Rect::new(0.0, 0.0, 10.0, 10.0).unwrap(),
            ImageFit::Cover,
        );
        let draw = resolve_image_draw(&model, 1, &node).unwrap();

        let expected_min_u = region.uv.min[0] + (region.uv.max[0] - region.uv.min[0]) * 0.25;
        let expected_max_u = region.uv.max[0] - (region.uv.max[0] - region.uv.min[0]) * 0.25;
        assert!((draw.atlas_uv[0] - expected_min_u).abs() < 1.0e-6);
        assert!((draw.atlas_uv[2] - expected_max_u).abs() < 1.0e-6);
        assert_eq!(draw.atlas_uv[1], region.uv.min[1]);
        assert_eq!(draw.atlas_uv[3], region.uv.max[1]);
    }

    #[test]
    fn missing_and_stale_handles_are_rejected() {
        let model = model();
        let region = model.regions().next().unwrap();
        let source = dimensions(region);
        let destination = Rect::new(0.0, 0.0, 10.0, 10.0).unwrap();
        let missing = ImageNode::new(
            ImageHandle::new(99, NonZeroU32::MIN),
            source,
            destination,
            ImageFit::Fill,
        );
        let stale = ImageNode::new(
            ImageHandle::new(region.handle.index, NonZeroU32::new(2).unwrap()),
            source,
            destination,
            ImageFit::Fill,
        );

        assert!(matches!(
            resolve_image_draw(&model, 1, &missing),
            Err(PrepareImageError::MissingHandle(_))
        ));
        assert!(matches!(
            resolve_image_draw(&model, 1, &stale),
            Err(PrepareImageError::StaleHandle(_))
        ));
    }

    #[test]
    fn instance_packing_preserves_painter_order() {
        let first = ImageDraw {
            page: 1,
            destination: [1.0, 2.0, 3.0, 4.0],
            atlas_uv: [0.1, 0.2, 0.3, 0.4],
        };
        let second = ImageDraw {
            page: 0,
            destination: [5.0, 6.0, 7.0, 8.0],
            atlas_uv: [0.5, 0.6, 0.7, 0.8],
        };
        let bytes = pack_instances(&[first, second]).unwrap();

        assert_eq!(bytes.len(), IMAGE_INSTANCE_SIZE * 2);
        assert_eq!(&bytes[..4], &1.0f32.to_le_bytes());
        assert_eq!(
            &bytes[IMAGE_INSTANCE_SIZE..IMAGE_INSTANCE_SIZE + 4],
            &5.0f32.to_le_bytes()
        );
    }
}
