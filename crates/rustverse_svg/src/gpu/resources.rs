//! Renderer-owned GPU resources that live across individual render requests.

use std::{
    collections::{BTreeMap, HashMap},
    sync::{
        Arc, Weak,
        atomic::{AtomicU64, Ordering},
    },
};

use anyhow::Context as _;

use crate::scene::{ImageHandle, ImageNode};

use super::atlas::{
    decode::{DecodeLimits, DecodedImage, decode_image},
    gpu::{
        AtlasUpload, GpuAtlasPage, GpuUploadMetrics, ImagePipeline, PrepareImageError,
        PreparedImageDraw, resolve_image_draw, scene_handle, upload_region,
    },
    packing::AtlasSetModel,
    source::BundledImage,
    types::{
        AssetClass, AssetKey, AtlasConfig, AtlasInsertKind, AtlasMetrics, ContentHash,
        DecodedAssetMetadata, ImageFormat,
    },
};
use super::startup::PreparedStartupAsset;

const DEFAULT_ATLAS_PAGE_SIZE: u32 = 2_048;
const DEFAULT_ATLAS_PADDING: u32 = 2;
const DEFAULT_ATLAS_MAX_PAGES: u32 = 32;
const DEFAULT_ATLAS_MAX_BYTES: u64 = 256 * 1024 * 1024;

/// Complete observable state of the persistent image atlas.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct ImageAtlasMetrics {
    pub(crate) constructions: u64,
    pub(crate) atlas: AtlasMetrics,
    pub(crate) upload_count: u64,
    pub(crate) upload_bytes: u64,
    pub(crate) runtime_requests: u64,
    pub(crate) runtime_hits: u64,
    pub(crate) runtime_misses: u64,
    pub(crate) runtime_insertions: u64,
    pub(crate) runtime_deduplicated: u64,
    pub(crate) runtime_versioned: u64,
    pub(crate) runtime_page_allocations: u64,
}

/// Persistent image textures, residency metadata, sampler, bind groups, and
/// reusable draw pipeline.
///
/// This type intentionally implements neither [`Clone`] nor [`Default`].
pub(crate) struct ImageAtlasSet {
    model: AtlasSetModel,
    pages: Vec<GpuAtlasPage>,
    bundled_handles: BTreeMap<BundledImage, ImageHandle>,
    pipeline: ImagePipeline,
    metrics: ImageAtlasMetrics,
}

impl ImageAtlasSet {
    pub(crate) fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        counts: &mut PersistentResourceCounts,
    ) -> anyhow::Result<Self> {
        let config = AtlasConfig::new(
            DEFAULT_ATLAS_PAGE_SIZE.min(device.limits().max_texture_dimension_2d),
            DEFAULT_ATLAS_PAGE_SIZE.min(device.limits().max_texture_dimension_2d),
            DEFAULT_ATLAS_PADDING,
            DEFAULT_ATLAS_MAX_PAGES,
            DEFAULT_ATLAS_MAX_BYTES,
        )
        .map_err(anyhow::Error::new)?;
        Self::new_with_config(device, queue, counts, config)
    }

    pub(crate) fn new_with_startup(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        counts: &mut PersistentResourceCounts,
        prepared: Vec<PreparedStartupAsset>,
    ) -> anyhow::Result<Self> {
        let config = AtlasConfig::new(
            DEFAULT_ATLAS_PAGE_SIZE.min(device.limits().max_texture_dimension_2d),
            DEFAULT_ATLAS_PAGE_SIZE.min(device.limits().max_texture_dimension_2d),
            DEFAULT_ATLAS_PADDING,
            DEFAULT_ATLAS_MAX_PAGES,
            DEFAULT_ATLAS_MAX_BYTES,
        )
        .map_err(anyhow::Error::new)?;
        let decoded_assets = prepared
            .into_iter()
            .map(|asset| (asset.key, asset.decoded))
            .collect();
        Self::new_with_decoded_assets(device, queue, counts, config, decoded_assets)
    }

    fn new_with_config(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        counts: &mut PersistentResourceCounts,
        config: AtlasConfig,
    ) -> anyhow::Result<Self> {
        let mut decoded_assets = Vec::with_capacity(BundledImage::all().len());
        for bundled in BundledImage::all() {
            let decoded = decode_image(bundled.encoded(), DecodeLimits::default())
                .with_context(|| format!("failed to decode bundled image {}", bundled.id()))?;
            let key = AssetKey::new(bundled.id()).map_err(anyhow::Error::new)?;
            decoded_assets.push((key, decoded));
        }
        Self::new_with_decoded_assets(device, queue, counts, config, decoded_assets)
    }

    fn new_with_decoded_assets(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        counts: &mut PersistentResourceCounts,
        config: AtlasConfig,
        decoded_assets: Vec<(AssetKey, DecodedImage)>,
    ) -> anyhow::Result<Self> {
        counts.image_atlas_sets += 1;
        let metadata = decoded_assets
            .iter()
            .map(|(key, decoded)| {
                DecodedAssetMetadata::new(
                    key.clone(),
                    decoded.content_hash,
                    decoded.width,
                    decoded.height,
                    ImageFormat::Rgba8Unorm,
                    AssetClass::StartupStatic,
                )
            })
            .collect::<Result<Vec<_>, _>>()
            .map_err(anyhow::Error::new)?;
        let model = AtlasSetModel::build_startup(config, metadata).map_err(anyhow::Error::new)?;

        for page in model.pages() {
            anyhow::ensure!(
                page.width <= device.limits().max_texture_dimension_2d
                    && page.height <= device.limits().max_texture_dimension_2d,
                "atlas page {} is {}x{}, exceeding max_texture_dimension_2d {}",
                page.index,
                page.width,
                page.height,
                device.limits().max_texture_dimension_2d
            );
        }
        let pages: Vec<_> = model
            .pages()
            .map(|page| GpuAtlasPage::new(device, page))
            .collect();

        let decoded_by_hash: HashMap<ContentHash, &DecodedImage> = decoded_assets
            .iter()
            .map(|(_, decoded)| (decoded.content_hash, decoded))
            .collect();
        let mut upload_metrics = GpuUploadMetrics::default();
        for region in model.regions() {
            let decoded = decoded_by_hash.get(&region.content_hash).with_context(|| {
                format!("decoded pixels missing for region {:?}", region.handle)
            })?;
            let page_metadata = model
                .pages()
                .nth(region.page as usize)
                .context("resident atlas page is missing")?;
            let page = pages
                .get(region.page as usize)
                .context("GPU atlas page is missing")?;
            let uploaded = upload_region(
                queue,
                page,
                page_metadata,
                config.padding,
                AtlasUpload { region, decoded },
            )?;
            upload_metrics.upload_count += 1;
            upload_metrics.upload_bytes = upload_metrics
                .upload_bytes
                .checked_add(uploaded)
                .context("atlas upload metrics overflowed")?;
        }

        let bundled_handles = BundledImage::all()
            .into_iter()
            .filter_map(|bundled| {
                let key = AssetKey::new(bundled.id()).expect("bundled asset key is valid");
                let handle = model.handle(&key).map(scene_handle)?;
                Some((bundled, handle))
            })
            .collect();
        let pipeline = ImagePipeline::new(device, &pages);
        let metrics = ImageAtlasMetrics {
            constructions: 1,
            atlas: model.metrics(),
            upload_count: upload_metrics.upload_count,
            upload_bytes: upload_metrics.upload_bytes,
            ..ImageAtlasMetrics::default()
        };

        // `decoded_assets` and all canonical RGBA bytes are dropped here.
        Ok(Self {
            model,
            pages,
            bundled_handles,
            pipeline,
            metrics,
        })
    }

    pub(crate) fn bundled_handle(&self, image: BundledImage) -> ImageHandle {
        self.bundled_handles[&image]
    }

    pub(crate) fn handle(&self, key: &AssetKey) -> Option<ImageHandle> {
        self.model.handle(key).map(scene_handle)
    }

    pub(crate) fn resident_image(
        &self,
        key: &AssetKey,
    ) -> Option<(ImageHandle, crate::scene::ImageDimensions)> {
        let handle = self.model.handle(key)?;
        let region = self.model.region(handle)?;
        Some((scene_handle(handle), super::atlas::gpu::dimensions(region)))
    }

    pub(crate) const fn metrics(&self) -> ImageAtlasMetrics {
        self.metrics
    }

    pub(crate) fn prepare_draws(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        viewport: [f32; 2],
        nodes: &[ImageNode],
    ) -> anyhow::Result<Vec<PreparedImageDraw>> {
        let draws = nodes
            .iter()
            .map(|node| resolve_image_draw(&self.model, self.pages.len(), node))
            .collect::<Result<Vec<_>, PrepareImageError>>()?;
        self.pipeline
            .prepare(device, queue, &self.pages, viewport, &draws)
    }

    /// Inserts one already-decoded runtime image into the persistent atlas.
    ///
    /// Calls are serialized by the renderer's single-owner `&mut self`
    /// boundary. A staged clone of the CPU model validates identity, atlas
    /// capacity, and page dimensions before the new residency is published.
    pub(crate) fn insert_runtime(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        key: AssetKey,
        decoded: DecodedImage,
    ) -> anyhow::Result<ImageHandle> {
        self.metrics.runtime_requests = self.metrics.runtime_requests.saturating_add(1);
        validate_decoded_runtime(&decoded)?;

        let old_page_count = self.pages.len();
        let old_region_count = self.model.regions().len();
        let mut staged = self.model.clone();
        let metadata = DecodedAssetMetadata::new(
            key,
            decoded.content_hash,
            decoded.width,
            decoded.height,
            ImageFormat::Rgba8Unorm,
            AssetClass::Dynamic,
        )
        .map_err(anyhow::Error::new)?;
        let outcome = staged
            .insert_runtime(metadata)
            .map_err(anyhow::Error::new)?;

        let versioned_new_region = match outcome.kind {
            AtlasInsertKind::Hit => {
                self.metrics.runtime_hits = self.metrics.runtime_hits.saturating_add(1);
                self.model = staged;
                self.metrics.atlas = self.model.metrics();
                return Ok(scene_handle(outcome.handle));
            }
            AtlasInsertKind::Deduplicated => {
                self.metrics.runtime_misses = self.metrics.runtime_misses.saturating_add(1);
                self.metrics.runtime_deduplicated =
                    self.metrics.runtime_deduplicated.saturating_add(1);
                debug_assert_eq!(staged.regions().len(), old_region_count);
                self.model = staged;
                self.metrics.atlas = self.model.metrics();
                return Ok(scene_handle(outcome.handle));
            }
            AtlasInsertKind::Versioned { deduplicated } => {
                self.metrics.runtime_misses = self.metrics.runtime_misses.saturating_add(1);
                if deduplicated {
                    self.metrics.runtime_versioned =
                        self.metrics.runtime_versioned.saturating_add(1);
                    self.metrics.runtime_deduplicated =
                        self.metrics.runtime_deduplicated.saturating_add(1);
                    debug_assert_eq!(staged.regions().len(), old_region_count);
                    self.model = staged;
                    self.metrics.atlas = self.model.metrics();
                    return Ok(scene_handle(outcome.handle));
                }
                true
            }
            AtlasInsertKind::Inserted => {
                self.metrics.runtime_misses = self.metrics.runtime_misses.saturating_add(1);
                false
            }
        };

        anyhow::ensure!(
            staged.regions().len() == old_region_count + 1,
            "runtime atlas insertion did not create exactly one resident region"
        );
        let new_region = staged
            .region(outcome.handle)
            .context("new runtime atlas region is unavailable")?;
        let new_pages_metadata: Vec<_> = staged.pages().skip(old_page_count).cloned().collect();
        for page in &new_pages_metadata {
            anyhow::ensure!(
                page.width <= device.limits().max_texture_dimension_2d
                    && page.height <= device.limits().max_texture_dimension_2d,
                "runtime atlas page {} is {}x{}, exceeding max_texture_dimension_2d {}",
                page.index,
                page.width,
                page.height,
                device.limits().max_texture_dimension_2d
            );
        }
        let new_pages: Vec<_> = new_pages_metadata
            .iter()
            .map(|page| GpuAtlasPage::new(device, page))
            .collect();

        let (gpu_page, page_metadata) = if (new_region.page as usize) < old_page_count {
            (
                self.pages
                    .get(new_region.page as usize)
                    .context("existing dynamic GPU atlas page is missing")?,
                staged
                    .pages()
                    .nth(new_region.page as usize)
                    .context("existing dynamic atlas page metadata is missing")?,
            )
        } else {
            let local_index = new_region.page as usize - old_page_count;
            (
                new_pages
                    .get(local_index)
                    .context("new dynamic GPU atlas page is missing")?,
                new_pages_metadata
                    .get(local_index)
                    .context("new dynamic atlas page metadata is missing")?,
            )
        };
        let uploaded = upload_region(
            queue,
            gpu_page,
            page_metadata,
            staged.config().padding,
            AtlasUpload {
                region: new_region,
                decoded: &decoded,
            },
        )?;

        let next_upload_bytes = self.metrics.upload_bytes.saturating_add(uploaded);
        let allocated_page_count = u64::try_from(new_pages_metadata.len()).unwrap_or(u64::MAX);
        self.pages.extend(new_pages);
        if !new_pages_metadata.is_empty() {
            self.pipeline.sync_pages(device, &self.pages);
        }
        self.model = staged;
        self.metrics.atlas = self.model.metrics();
        self.metrics.upload_count = self.metrics.upload_count.saturating_add(1);
        self.metrics.upload_bytes = next_upload_bytes;
        self.metrics.runtime_insertions = self.metrics.runtime_insertions.saturating_add(1);
        if versioned_new_region {
            self.metrics.runtime_versioned = self.metrics.runtime_versioned.saturating_add(1);
        }
        self.metrics.runtime_page_allocations = self
            .metrics
            .runtime_page_allocations
            .saturating_add(allocated_page_count);
        Ok(scene_handle(outcome.handle))
    }

    pub(crate) fn draw_prepared<'pass>(
        &'pass self,
        pass: &mut wgpu::RenderPass<'pass>,
        prepared: PreparedImageDraw,
    ) {
        self.pipeline.draw(pass, prepared);
    }
}

fn validate_decoded_runtime(decoded: &DecodedImage) -> anyhow::Result<()> {
    anyhow::ensure!(
        decoded.width != 0 && decoded.height != 0,
        "runtime decoded image dimensions must be non-zero"
    );
    let expected = u64::from(decoded.width)
        .checked_mul(u64::from(decoded.height))
        .and_then(|pixels| pixels.checked_mul(4))
        .context("runtime decoded image byte count overflowed")?;
    anyhow::ensure!(
        u64::try_from(decoded.rgba8.len()).context("runtime decoded image is too large")?
            == expected,
        "runtime decoded image has {} RGBA bytes; expected {expected}",
        decoded.rgba8.len()
    );
    let mut hash = ring::digest::Context::new(&ring::digest::SHA256);
    hash.update(&decoded.width.to_le_bytes());
    hash.update(&decoded.height.to_le_bytes());
    hash.update(&decoded.rgba8);
    let digest = hash.finish();
    anyhow::ensure!(
        digest.as_ref() == decoded.content_hash.0,
        "runtime decoded image content hash does not match its canonical pixels"
    );
    Ok(())
}

struct PreparedRuntimeImage {
    encoded_hash: [u8; 32],
    decoded: Weak<tokio::sync::OnceCell<Result<Arc<DecodedImage>, Arc<str>>>>,
}

/// Keeps a same-key preparation flight alive until the renderer owner has
/// serialized its GPU insertion.
pub(crate) struct PreparedRuntimeImageLease {
    decoded: Arc<DecodedImage>,
    _flight: Arc<tokio::sync::OnceCell<Result<Arc<DecodedImage>, Arc<str>>>>,
}

impl PreparedRuntimeImageLease {
    pub(crate) fn decoded(&self) -> &Arc<DecodedImage> {
        &self.decoded
    }
}

/// Coalesces concurrent encoded-image preparation before the renderer's
/// serialized GPU insertion boundary.
pub(crate) struct RuntimeImageCoordinator {
    prepared: tokio::sync::Mutex<HashMap<AssetKey, PreparedRuntimeImage>>,
    decode_count: Arc<AtomicU64>,
    decode_permits: Arc<tokio::sync::Semaphore>,
    limits: DecodeLimits,
}

impl RuntimeImageCoordinator {
    pub(crate) fn new(limits: DecodeLimits) -> Self {
        Self {
            prepared: tokio::sync::Mutex::new(HashMap::new()),
            decode_count: Arc::new(AtomicU64::new(0)),
            decode_permits: Arc::new(tokio::sync::Semaphore::new(4)),
            limits,
        }
    }

    pub(crate) async fn prepare(
        &self,
        key: AssetKey,
        encoded: Arc<[u8]>,
    ) -> anyhow::Result<PreparedRuntimeImageLease> {
        let digest = ring::digest::digest(&ring::digest::SHA256, &encoded);
        let mut encoded_hash = [0; 32];
        encoded_hash.copy_from_slice(digest.as_ref());
        let cell = {
            let mut prepared = self.prepared.lock().await;
            match prepared
                .get(&key)
                .map(|existing| (existing.encoded_hash, existing.decoded.upgrade()))
            {
                Some((existing_hash, Some(decoded))) if existing_hash == encoded_hash => decoded,
                Some((existing_hash, None)) if existing_hash == encoded_hash => {
                    let cell = Arc::new(tokio::sync::OnceCell::new());
                    prepared.insert(
                        key,
                        PreparedRuntimeImage {
                            encoded_hash,
                            decoded: Arc::downgrade(&cell),
                        },
                    );
                    cell
                }
                Some((_existing_hash, Some(_))) => {
                    anyhow::bail!(
                        "runtime preparation key {key} has concurrent requests with different encoded content"
                    );
                }
                Some((_, None)) | None => {
                    let cell = Arc::new(tokio::sync::OnceCell::new());
                    prepared.insert(
                        key,
                        PreparedRuntimeImage {
                            encoded_hash,
                            decoded: Arc::downgrade(&cell),
                        },
                    );
                    cell
                }
            }
        };
        let limits = self.limits;
        let decode_count = Arc::clone(&self.decode_count);
        let decode_permits = Arc::clone(&self.decode_permits);
        let result = cell
            .get_or_init(|| async move {
                let _permit = decode_permits
                    .acquire_owned()
                    .await
                    .map_err(|error| Arc::<str>::from(error.to_string()))?;
                decode_count.fetch_add(1, Ordering::Relaxed);
                tokio::task::spawn_blocking(move || {
                    let _permit = _permit;
                    decode_image(&encoded, limits)
                })
                .await
                .map_err(|error| Arc::<str>::from(error.to_string()))?
                .map(Arc::new)
                .map_err(|error| Arc::<str>::from(error.to_string()))
            })
            .await;
        let decoded = result.clone().map_err(|error| anyhow::anyhow!("{error}"))?;
        Ok(PreparedRuntimeImageLease {
            decoded,
            _flight: cell,
        })
    }

    pub(crate) fn decode_count(&self) -> u64 {
        self.decode_count.load(Ordering::Relaxed)
    }
}

/// Persistent text resources owned by the renderer.
pub(crate) struct GlyphonState;

impl GlyphonState {
    fn new(counts: &mut PersistentResourceCounts) -> Self {
        counts.glyphon_states += 1;
        Self
    }
}

/// Persistent registry of reusable GPU effect resources.
pub(crate) struct EffectRegistry;

impl EffectRegistry {
    fn new(counts: &mut PersistentResourceCounts) -> Self {
        counts.effect_registries += 1;
        Self
    }
}

/// Read-only construction counters for one persistent resource aggregate.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct PersistentResourceCounts {
    pub(crate) image_atlas_sets: usize,
    pub(crate) glyphon_states: usize,
    pub(crate) effect_registries: usize,
}

/// The complete set of long-lived resources owned by one renderer.
pub(crate) struct PersistentResources {
    image_atlases: ImageAtlasSet,
    _glyphon: GlyphonState,
    _effects: EffectRegistry,
    construction_counts: PersistentResourceCounts,
}

impl PersistentResources {
    pub(crate) fn new(device: &wgpu::Device, queue: &wgpu::Queue) -> anyhow::Result<Self> {
        let mut construction_counts = PersistentResourceCounts::default();
        let image_atlases = ImageAtlasSet::new(device, queue, &mut construction_counts)?;
        Ok(Self {
            image_atlases,
            _glyphon: GlyphonState::new(&mut construction_counts),
            _effects: EffectRegistry::new(&mut construction_counts),
            construction_counts,
        })
    }

    pub(crate) fn new_with_startup(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        prepared: Vec<PreparedStartupAsset>,
    ) -> anyhow::Result<Self> {
        let mut construction_counts = PersistentResourceCounts::default();
        let image_atlases =
            ImageAtlasSet::new_with_startup(device, queue, &mut construction_counts, prepared)?;
        Ok(Self {
            image_atlases,
            _glyphon: GlyphonState::new(&mut construction_counts),
            _effects: EffectRegistry::new(&mut construction_counts),
            construction_counts,
        })
    }

    pub(crate) fn image_atlases(&self) -> &ImageAtlasSet {
        &self.image_atlases
    }

    pub(crate) fn image_atlases_mut(&mut self) -> &mut ImageAtlasSet {
        &mut self.image_atlases
    }

    pub(crate) const fn construction_counts(&self) -> PersistentResourceCounts {
        self.construction_counts
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gpu::atlas::gpu::region_handle;

    fn runtime_fixture() -> (Arc<[u8]>, DecodedImage) {
        runtime_fixture_with_pixels([255, 0, 0, 255, 0, 255, 0, 128])
    }

    fn runtime_fixture_with_pixels(pixels: [u8; 8]) -> (Arc<[u8]>, DecodedImage) {
        let mut encoded = Vec::new();
        let mut encoder = png::Encoder::new(&mut encoded, 2, 1);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(&pixels).unwrap();
        drop(writer);
        let encoded: Arc<[u8]> = encoded.into();
        let decoded = decode_image(&encoded, DecodeLimits::default()).unwrap();
        (encoded, decoded)
    }

    fn runtime_solid(width: u32, height: u32, color: [u8; 4]) -> DecodedImage {
        let mut encoded = Vec::new();
        let mut encoder = png::Encoder::new(&mut encoded, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        let pixels = color.repeat((width * height) as usize);
        writer.write_image_data(&pixels).unwrap();
        drop(writer);
        decode_image(&encoded, DecodeLimits::default()).unwrap()
    }

    async fn device() -> Option<(wgpu::Device, wgpu::Queue)> {
        let instance = wgpu::Instance::default();
        let adapter = match instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::None,
                force_fallback_adapter: false,
                compatible_surface: None,
                apply_limit_buckets: false,
            })
            .await
        {
            Ok(adapter) => adapter,
            Err(error) => {
                if std::env::var_os("RUSTVERSE_REQUIRE_GPU").is_some() {
                    panic!("RUSTVERSE_REQUIRE_GPU is set but no adapter is available: {error}");
                }
                eprintln!("skipping GPU atlas test: {error}");
                return None;
            }
        };
        Some(
            adapter
                .request_device(&wgpu::DeviceDescriptor {
                    label: Some("rustverse_svg atlas test device"),
                    required_features: wgpu::Features::empty(),
                    required_limits: wgpu::Limits::default(),
                    memory_hints: wgpu::MemoryHints::MemoryUsage,
                    experimental_features: wgpu::ExperimentalFeatures::disabled(),
                    trace: wgpu::Trace::Off,
                })
                .await
                .unwrap(),
        )
    }

    #[tokio::test]
    async fn startup_constructs_singleton_and_preloads_every_bundled_image() {
        let Some((device, queue)) = device().await else {
            return;
        };
        let resources = PersistentResources::new(&device, &queue).unwrap();

        assert_eq!(
            resources.construction_counts(),
            PersistentResourceCounts {
                image_atlas_sets: 1,
                glyphon_states: 1,
                effect_registries: 1,
            }
        );
        let atlas = resources.image_atlases();
        for image in BundledImage::all() {
            let handle = atlas.bundled_handle(image);
            assert_eq!(
                atlas.handle(&AssetKey::new(image.id()).unwrap()),
                Some(handle)
            );
        }
        assert_eq!(atlas.metrics().constructions, 1);
        assert_eq!(atlas.metrics().atlas.asset_keys, 4);
        assert_eq!(
            atlas.metrics().upload_count,
            u64::from(atlas.metrics().atlas.regions)
        );
    }

    #[tokio::test]
    async fn warm_draw_preparation_does_not_upload_or_allocate_atlas_pages() {
        let Some((device, queue)) = device().await else {
            return;
        };
        let mut resources = PersistentResources::new(&device, &queue).unwrap();
        let handle = resources
            .image_atlases()
            .bundled_handle(BundledImage::StarIcon);
        let node = ImageNode::new(
            handle,
            crate::scene::ImageDimensions::new(48, 48).unwrap(),
            crate::scene::Rect::new(0.0, 0.0, 32.0, 32.0).unwrap(),
            crate::scene::ImageFit::Cover,
        );
        let before = resources.image_atlases().metrics();
        let first = resources
            .image_atlases_mut()
            .prepare_draws(&device, &queue, [32.0, 32.0], std::slice::from_ref(&node))
            .unwrap();
        let second = resources
            .image_atlases_mut()
            .prepare_draws(&device, &queue, [32.0, 32.0], &[node])
            .unwrap();

        assert_eq!(first, second);
        assert_eq!(resources.image_atlases().metrics(), before);
    }

    #[tokio::test]
    async fn resident_image_is_sampled_into_premultiplied_target() {
        let Some((device, queue)) = device().await else {
            return;
        };
        let mut resources = PersistentResources::new(&device, &queue).unwrap();
        let node = ImageNode::new(
            resources
                .image_atlases()
                .bundled_handle(BundledImage::StarIcon),
            crate::scene::ImageDimensions::new(48, 48).unwrap(),
            crate::scene::Rect::new(0.0, 0.0, 48.0, 48.0).unwrap(),
            crate::scene::ImageFit::Fill,
        );
        let prepared = resources
            .image_atlases_mut()
            .prepare_draws(&device, &queue, [48.0, 48.0], &[node])
            .unwrap();

        let target = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("rustverse_svg atlas sampling test target"),
            size: wgpu::Extent3d {
                width: 48,
                height: 48,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = target.create_view(&wgpu::TextureViewDescriptor::default());
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("rustverse_svg atlas sampling test readback"),
            size: 256 * 48,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("rustverse_svg atlas sampling test encoder"),
        });
        {
            let attachments = [Some(wgpu::RenderPassColorAttachment {
                view: &view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                    store: wgpu::StoreOp::Store,
                },
            })];
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("rustverse_svg atlas sampling test pass"),
                color_attachments: &attachments,
                ..Default::default()
            });
            resources
                .image_atlases()
                .draw_prepared(&mut pass, prepared[0]);
        }
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &target,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(256),
                    rows_per_image: Some(48),
                },
            },
            wgpu::Extent3d {
                width: 48,
                height: 48,
                depth_or_array_layers: 1,
            },
        );
        queue.submit([encoder.finish()]);

        let slice = readback.slice(..);
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        slice.map_async(wgpu::MapMode::Read, move |result| {
            let _ = sender.send(result);
        });
        device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
        receiver.recv().unwrap().unwrap();
        let mapped = slice.get_mapped_range().unwrap();
        let pixels: Vec<_> = mapped
            .chunks_exact(256)
            .flat_map(|row| row[..48 * 4].chunks_exact(4))
            .collect();

        assert!(pixels.iter().any(|pixel| pixel[3] != 0));
        assert!(
            pixels.iter().all(|pixel| {
                pixel[0] <= pixel[3] && pixel[1] <= pixel[3] && pixel[2] <= pixel[3]
            })
        );
    }

    #[tokio::test]
    async fn runtime_first_insert_uploads_once_and_warm_insert_is_a_hit() {
        let Some((device, queue)) = device().await else {
            return;
        };
        let mut resources = PersistentResources::new(&device, &queue).unwrap();
        let key = AssetKey::new("cache/runtime.png").unwrap();
        let (_, decoded) = runtime_fixture();
        let before = resources.image_atlases().metrics();
        let first = resources
            .image_atlases_mut()
            .insert_runtime(&device, &queue, key.clone(), decoded.clone())
            .unwrap();
        let inserted = resources.image_atlases().metrics();
        let warm = resources
            .image_atlases_mut()
            .insert_runtime(&device, &queue, key, decoded)
            .unwrap();
        let after = resources.image_atlases().metrics();

        assert_eq!(first, warm);
        assert_eq!(inserted.upload_count, before.upload_count + 1);
        assert_eq!(inserted.runtime_insertions, before.runtime_insertions + 1);
        assert_eq!(inserted.runtime_page_allocations, 1);
        assert_eq!(after.upload_count, inserted.upload_count);
        assert_eq!(after.atlas.pages, inserted.atlas.pages);
        assert_eq!(after.runtime_hits, inserted.runtime_hits + 1);
    }

    #[tokio::test]
    async fn runtime_content_dedup_aliases_key_without_upload() {
        let Some((device, queue)) = device().await else {
            return;
        };
        let mut resources = PersistentResources::new(&device, &queue).unwrap();
        let (_, decoded) = runtime_fixture();
        let first = resources
            .image_atlases_mut()
            .insert_runtime(
                &device,
                &queue,
                AssetKey::new("cache/a.png").unwrap(),
                decoded.clone(),
            )
            .unwrap();
        let inserted = resources.image_atlases().metrics();
        let alias = resources
            .image_atlases_mut()
            .insert_runtime(
                &device,
                &queue,
                AssetKey::new("cache/b.png").unwrap(),
                decoded,
            )
            .unwrap();
        let after = resources.image_atlases().metrics();

        assert_eq!(first, alias);
        assert_eq!(after.upload_count, inserted.upload_count);
        assert_eq!(after.atlas.pages, inserted.atlas.pages);
        assert_eq!(
            after.runtime_deduplicated,
            inserted.runtime_deduplicated + 1
        );
    }

    #[tokio::test]
    async fn runtime_changed_key_repoints_and_keeps_old_region_live() {
        let Some((device, queue)) = device().await else {
            return;
        };
        let mut resources = PersistentResources::new(&device, &queue).unwrap();
        let key = AssetKey::new("cache/versioned.png").unwrap();
        let (_, first_pixels) = runtime_fixture();
        let (_, second_pixels) = runtime_fixture_with_pixels([0, 0, 255, 255, 255, 255, 0, 255]);
        let first = resources
            .image_atlases_mut()
            .insert_runtime(&device, &queue, key.clone(), first_pixels)
            .unwrap();
        let before = resources.image_atlases().metrics();
        let second = resources
            .image_atlases_mut()
            .insert_runtime(&device, &queue, key.clone(), second_pixels)
            .unwrap();
        let after = resources.image_atlases().metrics();

        assert_ne!(first, second);
        assert_eq!(resources.image_atlases().handle(&key), Some(second));
        assert!(
            resources
                .image_atlases()
                .model
                .region(region_handle(first))
                .is_some()
        );
        assert_eq!(after.runtime_versioned, before.runtime_versioned + 1);
        assert_eq!(after.runtime_insertions, before.runtime_insertions + 1);
        assert_eq!(after.upload_count, before.upload_count + 1);
    }

    #[tokio::test]
    async fn runtime_changed_key_can_deduplicate_existing_version() {
        let Some((device, queue)) = device().await else {
            return;
        };
        let mut resources = PersistentResources::new(&device, &queue).unwrap();
        let key = AssetKey::new("cache/versioned.png").unwrap();
        let (_, first_pixels) = runtime_fixture();
        let (_, second_pixels) = runtime_fixture_with_pixels([0, 0, 255, 255, 255, 255, 0, 255]);
        let old = resources
            .image_atlases_mut()
            .insert_runtime(&device, &queue, key.clone(), first_pixels)
            .unwrap();
        let existing = resources
            .image_atlases_mut()
            .insert_runtime(
                &device,
                &queue,
                AssetKey::new("cache/existing.png").unwrap(),
                second_pixels.clone(),
            )
            .unwrap();
        let before = resources.image_atlases().metrics();
        let versioned = resources
            .image_atlases_mut()
            .insert_runtime(&device, &queue, key.clone(), second_pixels)
            .unwrap();
        let after = resources.image_atlases().metrics();

        assert_ne!(old, versioned);
        assert_eq!(versioned, existing);
        assert_eq!(resources.image_atlases().handle(&key), Some(existing));
        assert!(
            resources
                .image_atlases()
                .model
                .region(region_handle(old))
                .is_some()
        );
        assert_eq!(after.upload_count, before.upload_count);
        assert_eq!(after.runtime_versioned, before.runtime_versioned + 1);
        assert_eq!(after.runtime_deduplicated, before.runtime_deduplicated + 1);
    }

    #[tokio::test]
    async fn runtime_page_capacity_failure_does_not_publish_handle() {
        let Some((device, queue)) = device().await else {
            return;
        };
        let mut counts = PersistentResourceCounts::default();
        let initial = ImageAtlasSet::new(&device, &queue, &mut counts).unwrap();
        let startup_pages = initial.metrics().atlas.pages;
        drop(initial);
        let config = AtlasConfig::new(
            DEFAULT_ATLAS_PAGE_SIZE,
            DEFAULT_ATLAS_PAGE_SIZE,
            DEFAULT_ATLAS_PADDING,
            startup_pages,
            DEFAULT_ATLAS_MAX_BYTES,
        )
        .unwrap();
        let mut counts = PersistentResourceCounts::default();
        let mut atlas =
            ImageAtlasSet::new_with_config(&device, &queue, &mut counts, config).unwrap();
        let key = AssetKey::new("cache/over-capacity.png").unwrap();
        let (_, decoded) = runtime_fixture();

        let error = atlas
            .insert_runtime(&device, &queue, key.clone(), decoded)
            .unwrap_err();

        assert!(
            error.to_string().contains("page capacity"),
            "unexpected error: {error:#}"
        );
        assert!(atlas.handle(&key).is_none());
        assert_eq!(atlas.metrics().atlas.pages, startup_pages);
    }

    #[tokio::test]
    async fn failed_runtime_version_keeps_previous_mapping() {
        let Some((device, queue)) = device().await else {
            return;
        };
        let mut counts = PersistentResourceCounts::default();
        let initial = ImageAtlasSet::new(&device, &queue, &mut counts).unwrap();
        let startup_pages = initial.metrics().atlas.pages;
        drop(initial);
        let config = AtlasConfig::new(
            DEFAULT_ATLAS_PAGE_SIZE,
            DEFAULT_ATLAS_PAGE_SIZE,
            DEFAULT_ATLAS_PADDING,
            startup_pages + 1,
            DEFAULT_ATLAS_MAX_BYTES,
        )
        .unwrap();
        let mut counts = PersistentResourceCounts::default();
        let mut atlas =
            ImageAtlasSet::new_with_config(&device, &queue, &mut counts, config).unwrap();
        let key = AssetKey::new("cache/version-capacity.png").unwrap();
        let first_pixels = runtime_solid(DEFAULT_ATLAS_PAGE_SIZE, 1, [255, 0, 0, 255]);
        let second_pixels = runtime_solid(DEFAULT_ATLAS_PAGE_SIZE, 1, [0, 0, 255, 255]);
        let first = atlas
            .insert_runtime(&device, &queue, key.clone(), first_pixels)
            .unwrap();
        let before = atlas.metrics();

        let error = atlas
            .insert_runtime(&device, &queue, key.clone(), second_pixels)
            .unwrap_err();

        assert!(error.to_string().contains("page capacity"));
        assert_eq!(atlas.handle(&key), Some(first));
        assert!(atlas.model.region(region_handle(first)).is_some());
        assert_eq!(atlas.metrics().atlas, before.atlas);
        assert_eq!(atlas.metrics().runtime_versioned, before.runtime_versioned);
        assert_eq!(atlas.metrics().upload_count, before.upload_count);
    }

    #[tokio::test]
    async fn concurrent_preparation_and_serialized_insertion_upload_once() {
        let Some((device, queue)) = device().await else {
            return;
        };
        let coordinator = RuntimeImageCoordinator::new(DecodeLimits::default());
        let key = AssetKey::new("cache/coalesced.png").unwrap();
        let (encoded, _) = runtime_fixture();
        let (first, second) = tokio::join!(
            coordinator.prepare(key.clone(), Arc::clone(&encoded)),
            coordinator.prepare(key.clone(), encoded),
        );
        let first = first.unwrap();
        let second = second.unwrap();

        assert!(Arc::ptr_eq(first.decoded(), second.decoded()));
        assert_eq!(coordinator.decode_count(), 1);

        let mut resources = PersistentResources::new(&device, &queue).unwrap();
        let before = resources.image_atlases().metrics();
        let first_handle = resources
            .image_atlases_mut()
            .insert_runtime(&device, &queue, key.clone(), (**first.decoded()).clone())
            .unwrap();
        let second_handle = resources
            .image_atlases_mut()
            .insert_runtime(&device, &queue, key, (**second.decoded()).clone())
            .unwrap();
        let after = resources.image_atlases().metrics();

        assert_eq!(first_handle, second_handle);
        assert_eq!(after.upload_count, before.upload_count + 1);
        assert_eq!(after.runtime_insertions, before.runtime_insertions + 1);
        assert_eq!(after.runtime_hits, before.runtime_hits + 1);
    }

    #[tokio::test]
    async fn runtime_coordinator_allows_changed_content_after_completed_flight() {
        let coordinator = RuntimeImageCoordinator::new(DecodeLimits::default());
        let key = AssetKey::new("cache/versioned-flight.png").unwrap();
        let (first_encoded, _) = runtime_fixture();
        let (second_encoded, _) = runtime_fixture_with_pixels([0, 0, 255, 255, 255, 255, 0, 255]);

        let first = coordinator
            .prepare(key.clone(), first_encoded)
            .await
            .unwrap();
        drop(first);
        tokio::task::yield_now().await;
        let second = coordinator.prepare(key, second_encoded).await.unwrap();

        assert_eq!(coordinator.decode_count(), 2);
        assert_eq!(second.decoded().rgba8, [0, 0, 255, 255, 255, 255, 0, 255]);
    }

    #[tokio::test]
    async fn runtime_image_is_sampled_from_dynamic_page() {
        let Some((device, queue)) = device().await else {
            return;
        };
        let mut resources = PersistentResources::new(&device, &queue).unwrap();
        let (_, decoded) = runtime_fixture();
        let handle = resources
            .image_atlases_mut()
            .insert_runtime(
                &device,
                &queue,
                AssetKey::new("cache/sample.png").unwrap(),
                decoded,
            )
            .unwrap();
        let node = ImageNode::new(
            handle,
            crate::scene::ImageDimensions::new(2, 1).unwrap(),
            crate::scene::Rect::new(0.0, 0.0, 2.0, 1.0).unwrap(),
            crate::scene::ImageFit::Fill,
        );
        let prepared = resources
            .image_atlases_mut()
            .prepare_draws(&device, &queue, [2.0, 1.0], &[node])
            .unwrap();

        assert_eq!(prepared.len(), 1);
        assert_eq!(
            prepared[0].page + 1,
            resources.image_atlases().metrics().atlas.pages
        );

        let target = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("rustverse_svg runtime atlas sampling target"),
            size: wgpu::Extent3d {
                width: 2,
                height: 1,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = target.create_view(&wgpu::TextureViewDescriptor::default());
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("rustverse_svg runtime atlas sampling readback"),
            size: 256,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("rustverse_svg runtime atlas sampling encoder"),
        });
        {
            let attachments = [Some(wgpu::RenderPassColorAttachment {
                view: &view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                    store: wgpu::StoreOp::Store,
                },
            })];
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("rustverse_svg runtime atlas sampling pass"),
                color_attachments: &attachments,
                ..Default::default()
            });
            resources
                .image_atlases()
                .draw_prepared(&mut pass, prepared[0]);
        }
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &target,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(256),
                    rows_per_image: Some(1),
                },
            },
            wgpu::Extent3d {
                width: 2,
                height: 1,
                depth_or_array_layers: 1,
            },
        );
        queue.submit([encoder.finish()]);
        let slice = readback.slice(..);
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        slice.map_async(wgpu::MapMode::Read, move |result| {
            let _ = sender.send(result);
        });
        device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
        receiver.recv().unwrap().unwrap();
        let mapped = slice.get_mapped_range().unwrap();

        assert_eq!(&mapped[..4], &[255, 0, 0, 255]);
        assert_eq!(&mapped[4..8], &[0, 128, 0, 128]);
    }
}
