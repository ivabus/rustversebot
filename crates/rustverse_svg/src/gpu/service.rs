//! GPU-specific single-owner service and runtime resource control plane.

use std::{collections::BTreeMap, sync::Arc};

use tokio::sync::{mpsc, oneshot};

use crate::{
    renderer_service::{RenderRequest, RenderServiceError},
    scene::{ImageDimensions, ImageHandle},
};

use super::resources::{ImageAtlasMetrics, PreparedRuntimeImageLease};
use super::{
    GpuRenderError, GpuRenderer, atlas::types::AssetKey, resources::RuntimeImageCoordinator,
    startup::BundledAsset,
};

/// A renderer-owned image that can safely be referenced by scene nodes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResidentImage {
    pub handle: ImageHandle,
    pub dimensions: ImageDimensions,
}

/// Read-only snapshot of persistent GPU image-atlas usage and runtime activity.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct GpuRendererMetrics {
    pub pages: u32,
    pub pinned_pages: u32,
    pub oversize_pages: u32,
    pub allocated_pixels: u64,
    pub allocated_bytes: u64,
    pub occupied_pixels: u64,
    pub occupied_bytes: u64,
    pub usable_pixels: u64,
    pub usable_bytes: u64,
    pub configured_max_pages: u32,
    pub configured_max_bytes: u64,
    pub remaining_pages: u32,
    pub remaining_bytes: u64,
    pub regions: u32,
    pub asset_keys: u32,
    pub deduplicated_assets: u64,
    pub upload_count: u64,
    pub upload_bytes: u64,
    pub runtime_requests: u64,
    pub runtime_hits: u64,
    pub runtime_misses: u64,
    pub runtime_insertions: u64,
    pub runtime_versioned: u64,
    pub runtime_deduplicated: u64,
    pub runtime_page_allocations: u64,
}

impl From<ImageAtlasMetrics> for GpuRendererMetrics {
    fn from(metrics: ImageAtlasMetrics) -> Self {
        let atlas = metrics.atlas;
        Self {
            pages: atlas.pages,
            pinned_pages: atlas.pinned_pages,
            oversize_pages: atlas.oversize_pages,
            allocated_pixels: atlas.allocated_bytes / 4,
            allocated_bytes: atlas.allocated_bytes,
            occupied_pixels: atlas.occupied_pixels,
            occupied_bytes: atlas.occupied_pixels * 4,
            usable_pixels: atlas.usable_pixels,
            usable_bytes: atlas.usable_pixels * 4,
            configured_max_pages: atlas.configured_max_pages,
            configured_max_bytes: atlas.configured_max_bytes,
            remaining_pages: atlas.remaining_pages,
            remaining_bytes: atlas.remaining_bytes,
            regions: atlas.regions,
            asset_keys: atlas.asset_keys,
            deduplicated_assets: atlas.deduplicated_assets,
            upload_count: metrics.upload_count,
            upload_bytes: metrics.upload_bytes,
            runtime_requests: metrics.runtime_requests,
            runtime_hits: metrics.runtime_hits,
            runtime_misses: metrics.runtime_misses,
            runtime_insertions: metrics.runtime_insertions,
            runtime_versioned: metrics.runtime_versioned,
            runtime_deduplicated: metrics.runtime_deduplicated,
            runtime_page_allocations: metrics.runtime_page_allocations,
        }
    }
}

enum OwnerCommand {
    Render {
        request: RenderRequest,
        response: oneshot::Sender<Result<Vec<u8>, GpuRenderError>>,
    },
    InsertPreparedImage {
        key: AssetKey,
        prepared: PreparedRuntimeImageLease,
        response: oneshot::Sender<Result<ResidentImage, GpuRenderError>>,
    },
    Metrics {
        response: oneshot::Sender<GpuRendererMetrics>,
    },
    #[cfg(test)]
    Pause {
        entered: oneshot::Sender<()>,
        release: oneshot::Receiver<()>,
    },
}

/// Cloneable control plane for one long-lived, single-owner GPU renderer.
///
/// Encoded input is decoded and coalesced before it enters the bounded owner
/// queue. The owner serializes atlas mutation with rendering, so no draw can
/// observe a partially published runtime region.
pub struct GpuRendererService {
    sender: mpsc::Sender<OwnerCommand>,
    runtime_images: Arc<RuntimeImageCoordinator>,
    startup: Arc<BTreeMap<String, ResidentImage>>,
}

impl Clone for GpuRendererService {
    fn clone(&self) -> Self {
        Self {
            sender: self.sender.clone(),
            runtime_images: Arc::clone(&self.runtime_images),
            startup: Arc::clone(&self.startup),
        }
    }
}

impl GpuRendererService {
    pub(crate) async fn start(
        options: super::GpuRendererOptions,
        queue_capacity: usize,
    ) -> Result<Self, GpuRenderError> {
        assert!(
            queue_capacity > 0,
            "renderer service queue capacity must be greater than zero"
        );
        let startup_keys = options
            .startup_manifest()
            .entries()
            .iter()
            .map(|entry| entry.key().to_owned())
            .collect::<Vec<_>>();
        let renderer = GpuRenderer::new(options)
            .await
            .map_err(GpuRenderError::Initialize)?;
        let startup = Arc::new(renderer.startup_registry(startup_keys)?);
        let (sender, receiver) = mpsc::channel(queue_capacity);
        let runtime = tokio::runtime::Handle::current();
        std::thread::Builder::new()
            .name("rustverse-svg-gpu-renderer".to_owned())
            .spawn(move || runtime.block_on(run_owner(renderer, receiver)))
            .expect("failed to start GPU renderer owner thread");
        Ok(Self {
            sender,
            runtime_images: Arc::new(RuntimeImageCoordinator::new(
                super::atlas::decode::DecodeLimits::default(),
            )),
            startup,
        })
    }

    /// Returns a startup-resident bundled asset without touching the owner.
    pub fn bundled_asset(&self, asset: BundledAsset) -> Option<ResidentImage> {
        self.startup_asset(asset.stable_key())
    }

    /// Looks up any configured startup manifest entry without touching the owner.
    pub fn startup_asset(&self, key: &str) -> Option<ResidentImage> {
        self.startup.get(key).copied()
    }

    /// Renders through the same bounded owner queue used for atlas insertion.
    pub async fn render(
        &self,
        request: RenderRequest,
    ) -> Result<Vec<u8>, RenderServiceError<GpuRenderError>> {
        let (response, receiver) = oneshot::channel();
        self.sender
            .send(OwnerCommand::Render { request, response })
            .await
            .map_err(|_| RenderServiceError::Unavailable)?;
        receiver
            .await
            .map_err(|_| RenderServiceError::ResponseDropped)?
            .map_err(RenderServiceError::Backend)
    }

    /// Ensures encoded content is resident and returns an opaque scene handle.
    pub async fn ensure_image(
        &self,
        key: impl Into<String>,
        encoded: Arc<[u8]>,
    ) -> Result<ResidentImage, RenderServiceError<GpuRenderError>> {
        let key = AssetKey::new(key.into()).map_err(|error| {
            RenderServiceError::Backend(GpuRenderError::Render(anyhow::Error::new(error)))
        })?;
        let prepared = self
            .runtime_images
            .prepare(key.clone(), encoded)
            .await
            .map_err(|error| RenderServiceError::Backend(GpuRenderError::Render(error)))?;
        let (response, receiver) = oneshot::channel();
        self.sender
            .send(OwnerCommand::InsertPreparedImage {
                key,
                prepared,
                response,
            })
            .await
            .map_err(|_| RenderServiceError::Unavailable)?;
        receiver
            .await
            .map_err(|_| RenderServiceError::ResponseDropped)?
            .map_err(RenderServiceError::Backend)
    }

    pub fn available_queue_capacity(&self) -> usize {
        self.sender.capacity()
    }

    pub fn queue_capacity(&self) -> usize {
        self.sender.max_capacity()
    }

    /// Returns an owner-serialized snapshot of persistent atlas metrics.
    pub async fn metrics(&self) -> Result<GpuRendererMetrics, RenderServiceError<GpuRenderError>> {
        let (response, receiver) = oneshot::channel();
        self.sender
            .send(OwnerCommand::Metrics { response })
            .await
            .map_err(|_| RenderServiceError::Unavailable)?;
        receiver
            .await
            .map_err(|_| RenderServiceError::ResponseDropped)
    }
}

async fn run_owner(mut renderer: GpuRenderer, mut receiver: mpsc::Receiver<OwnerCommand>) {
    while let Some(command) = receiver.recv().await {
        match command {
            OwnerCommand::Render { request, response } => {
                if response.is_closed() {
                    continue;
                }
                let result = renderer
                    .render_request(request)
                    .await
                    .map(|image| image.png)
                    .map_err(GpuRenderError::Render);
                let _ = response.send(result);
            }
            OwnerCommand::InsertPreparedImage {
                key,
                prepared,
                response,
            } => {
                if response.is_closed() {
                    continue;
                }
                let result = renderer
                    .insert_prepared_image(key, Arc::clone(prepared.decoded()))
                    .map_err(GpuRenderError::Render);
                let _ = response.send(result);
            }
            OwnerCommand::Metrics { response } => {
                let snapshot = renderer.resources.image_atlases().metrics().into();
                let _ = response.send(snapshot);
            }
            #[cfg(test)]
            OwnerCommand::Pause { entered, release } => {
                let _ = entered.send(());
                let _ = release.await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::{
        RenderScale,
        gpu::{
            GpuInitError, GpuRendererOptions,
            startup::{StartupAssetEntry, StartupAssetManifest},
        },
        renderer_service::SolidColor,
        scene::{ImageFit, ImageNode, LogicalSize, Rect, Scene, SceneNode},
    };

    fn png(width: u32, height: u32, rgba: &[u8]) -> Arc<[u8]> {
        let mut encoded = Vec::new();
        let mut encoder = png::Encoder::new(&mut encoded, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(rgba).unwrap();
        drop(writer);
        encoded.into()
    }

    async fn service_or_skip(options: GpuRendererOptions) -> Option<GpuRendererService> {
        match GpuRendererService::start(options, 8).await {
            Ok(service) => Some(service),
            Err(GpuRenderError::Initialize(GpuInitError::AdapterUnavailable(error))) => {
                if std::env::var_os("RUSTVERSE_REQUIRE_GPU").is_some_and(|value| value == "1") {
                    panic!("RUSTVERSE_REQUIRE_GPU=1 but no GPU adapter is available: {error}");
                }
                eprintln!("SKIP: no surface-free GPU adapter is available: {error}");
                None
            }
            Err(error) => panic!("GPU renderer service initialization failed: {error}"),
        }
    }

    async fn pause_owner(service: &GpuRendererService) -> oneshot::Sender<()> {
        let (entered, entered_receiver) = oneshot::channel();
        let (release, release_receiver) = oneshot::channel();
        service
            .sender
            .send(OwnerCommand::Pause {
                entered,
                release: release_receiver,
            })
            .await
            .unwrap();
        entered_receiver.await.unwrap();
        release
    }

    async fn wait_for_queue_capacity(service: &GpuRendererService, expected: usize) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while service.available_queue_capacity() != expected {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("owner command was not queued");
    }

    async fn render_images(
        service: &GpuRendererService,
        images: impl IntoIterator<Item = (ResidentImage, Rect)>,
        logical_size: LogicalSize,
    ) -> Vec<u8> {
        let mut scene = Scene::<SceneNode>::new(logical_size);
        for (resident, destination) in images {
            scene.nodes.push(
                ImageNode::new(
                    resident.handle,
                    resident.dimensions,
                    destination,
                    ImageFit::Fill,
                )
                .into(),
            );
        }
        service
            .render(RenderRequest::scene(
                scene,
                RenderScale::ONE,
                SolidColor::rgba(0, 0, 0, 0),
            ))
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn caller_gets_bundled_handle_and_renders_it() {
        let Some(service) = service_or_skip(GpuRendererOptions::default()).await else {
            return;
        };
        let resident = service
            .bundled_asset(BundledAsset::StarIcon)
            .expect("default manifest contains the star");
        let png = render_images(
            &service,
            [(resident, Rect::new(0.0, 0.0, 48.0, 48.0).unwrap())],
            LogicalSize {
                width: 48.0,
                height: 48.0,
            },
        )
        .await;
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
    }

    #[tokio::test]
    async fn concurrent_ensure_coalesces_decode_and_gpu_insertion() {
        let Some(service) = service_or_skip(GpuRendererOptions::default()).await else {
            return;
        };
        let encoded = png(2, 1, &[255, 0, 0, 255, 0, 255, 0, 255]);
        let before = service.metrics().await.unwrap();
        let decode_before = service.runtime_images.decode_count();
        let (first, second) = tokio::join!(
            service.ensure_image("runtime/shared.png", Arc::clone(&encoded)),
            service.ensure_image("runtime/shared.png", encoded),
        );
        let first = first.unwrap();
        let second = second.unwrap();
        let after = service.metrics().await.unwrap();

        assert_eq!(first, second);
        assert_eq!(service.runtime_images.decode_count(), decode_before + 1);
        assert_eq!(after.upload_count, before.upload_count + 1);
        assert_eq!(after.runtime_insertions, before.runtime_insertions + 1);
    }

    #[tokio::test]
    async fn preparation_lease_blocks_conflicting_version_until_owner_publication() {
        let Some(service) = service_or_skip(GpuRendererOptions::default()).await else {
            return;
        };
        let release = pause_owner(&service).await;
        let initial_capacity = service.queue_capacity();
        let decode_before = service.runtime_images.decode_count();
        let old_encoded = png(1, 1, &[255, 0, 0, 255]);
        let new_encoded = png(1, 1, &[0, 255, 0, 255]);

        let first_service = service.clone();
        let first_encoded = Arc::clone(&old_encoded);
        let first = tokio::spawn(async move {
            first_service
                .ensure_image("runtime/leased-version.png", first_encoded)
                .await
        });
        wait_for_queue_capacity(&service, initial_capacity - 1).await;

        let second_service = service.clone();
        let second = tokio::spawn(async move {
            second_service
                .ensure_image("runtime/leased-version.png", old_encoded)
                .await
        });
        wait_for_queue_capacity(&service, initial_capacity - 2).await;
        assert_eq!(service.runtime_images.decode_count(), decode_before + 1);

        let conflict = service
            .ensure_image("runtime/leased-version.png", Arc::clone(&new_encoded))
            .await
            .unwrap_err();
        assert!(
            conflict
                .to_string()
                .contains("concurrent requests with different encoded content")
        );

        release.send(()).unwrap();
        let first = first.await.unwrap().unwrap();
        let second = second.await.unwrap().unwrap();
        assert_eq!(first, second);

        let new = service
            .ensure_image("runtime/leased-version.png", new_encoded)
            .await
            .unwrap();
        assert_ne!(new.handle, first.handle);
        assert_eq!(service.runtime_images.decode_count(), decode_before + 2);
    }

    #[tokio::test]
    async fn public_metrics_snapshot_reports_atlas_and_runtime_usage() {
        let Some(service) = service_or_skip(GpuRendererOptions::default()).await else {
            return;
        };
        let before = service.metrics().await.unwrap();
        assert!(before.pages > 0);
        assert!(before.pinned_pages > 0);
        assert_eq!(before.allocated_bytes, before.allocated_pixels * 4);
        assert_eq!(before.occupied_bytes, before.occupied_pixels * 4);
        assert_eq!(before.usable_bytes, before.usable_pixels * 4);
        assert_eq!(
            before.pages + before.remaining_pages,
            before.configured_max_pages
        );
        assert_eq!(
            before.allocated_bytes + before.remaining_bytes,
            before.configured_max_bytes
        );

        service
            .ensure_image("runtime/metrics.png", png(1, 1, &[1, 2, 3, 255]))
            .await
            .unwrap();
        let after = service.metrics().await.unwrap();
        assert_eq!(after.runtime_requests, before.runtime_requests + 1);
        assert_eq!(after.runtime_misses, before.runtime_misses + 1);
        assert_eq!(after.runtime_insertions, before.runtime_insertions + 1);
        assert_eq!(after.upload_count, before.upload_count + 1);
        assert!(after.upload_bytes > before.upload_bytes);
        assert_eq!(after.asset_keys, before.asset_keys + 1);
        assert_eq!(after.regions, before.regions + 1);
    }

    #[tokio::test]
    async fn changed_stable_key_versions_handle_and_keeps_old_handle_live() {
        let Some(service) = service_or_skip(GpuRendererOptions::default()).await else {
            return;
        };
        let old = service
            .ensure_image("runtime/versioned.png", png(1, 1, &[255, 0, 0, 255]))
            .await
            .unwrap();
        let new = service
            .ensure_image("runtime/versioned.png", png(1, 1, &[0, 255, 0, 255]))
            .await
            .unwrap();
        assert_ne!(old.handle, new.handle);

        let rendered = render_images(
            &service,
            [
                (old, Rect::new(0.0, 0.0, 1.0, 1.0).unwrap()),
                (new, Rect::new(1.0, 0.0, 1.0, 1.0).unwrap()),
            ],
            LogicalSize {
                width: 2.0,
                height: 1.0,
            },
        )
        .await;
        assert_eq!(&rendered[..8], b"\x89PNG\r\n\x1a\n");
    }

    #[tokio::test]
    async fn configured_cached_startup_asset_is_immediately_renderable() {
        let root = std::env::temp_dir().join(format!(
            "rustverse-svg-service-startup-{}",
            std::process::id()
        ));
        tokio::fs::create_dir_all(&root).await.unwrap();
        tokio::fs::write(root.join("boss.png"), &*png(1, 1, &[20, 40, 60, 255]))
            .await
            .unwrap();
        let manifest = StartupAssetManifest::new(
            [StartupAssetEntry::cached("boss/current", &root, "boss.png").unwrap()],
            1,
        )
        .unwrap();
        let options = GpuRendererOptions::default().with_startup_manifest(manifest);
        let Some(service) = service_or_skip(options).await else {
            tokio::fs::remove_dir_all(root).await.unwrap();
            return;
        };
        let resident = service
            .startup_asset("boss/current")
            .expect("configured startup asset must be resident");
        let before = service.metrics().await.unwrap();
        let _ = render_images(
            &service,
            [(resident, Rect::new(0.0, 0.0, 1.0, 1.0).unwrap())],
            LogicalSize {
                width: 1.0,
                height: 1.0,
            },
        )
        .await;
        assert_eq!(service.metrics().await.unwrap(), before);
        tokio::fs::remove_dir_all(root).await.unwrap();
    }
}
