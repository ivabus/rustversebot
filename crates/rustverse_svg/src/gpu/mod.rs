//! Headless GPU rendering infrastructure.

mod context;
pub mod resources;

use std::fmt;

use context::{GpuContext, HeadlessImage};
pub use context::{GpuInitError, PhysicalSize, RGBA8_TARGET_FORMAT, physical_size};
use resources::PersistentResources;

use crate::renderer_service::{RenderRequest, RendererBackend, RendererService};

/// Default upper bound for both RGBA output and padded staging allocations.
pub const DEFAULT_MAX_TARGET_BYTES: u64 = 256 * 1024 * 1024;

/// Hard upper bound accepted by [`GpuRendererOptions`].
pub const MAX_CONFIGURED_TARGET_BYTES: u64 = 1024 * 1024 * 1024;

/// Validated startup options for the single-owner GPU renderer service.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GpuRendererOptions {
    max_target_bytes: u64,
}

impl GpuRendererOptions {
    pub fn new(max_target_bytes: u64) -> anyhow::Result<Self> {
        anyhow::ensure!(max_target_bytes > 0, "max_target_bytes must be non-zero");
        anyhow::ensure!(
            max_target_bytes <= MAX_CONFIGURED_TARGET_BYTES,
            "max_target_bytes {max_target_bytes} exceeds configured upper bound {MAX_CONFIGURED_TARGET_BYTES}"
        );
        Ok(Self { max_target_bytes })
    }

    pub const fn max_target_bytes(self) -> u64 {
        self.max_target_bytes
    }
}

impl Default for GpuRendererOptions {
    fn default() -> Self {
        Self {
            max_target_bytes: DEFAULT_MAX_TARGET_BYTES,
        }
    }
}

/// The Phase 2 renderer owns one GPU context and one set of persistent
/// renderer resources for its complete lifetime.
pub(crate) struct GpuRenderer {
    context: GpuContext,
    _resources: PersistentResources,
    options: GpuRendererOptions,
}

impl GpuRenderer {
    async fn new(options: GpuRendererOptions) -> Result<Self, GpuInitError> {
        let context = GpuContext::new().await?;
        let resources = PersistentResources::new();
        debug_assert_eq!(
            resources.construction_counts(),
            resources::PersistentResourceCounts {
                image_atlas_sets: 1,
                glyphon_states: 1,
                effect_registries: 1,
            }
        );
        Ok(Self {
            context,
            _resources: resources,
            options,
        })
    }

    async fn render_clear(&self, request: RenderRequest) -> Result<HeadlessImage, anyhow::Error> {
        let color = request.color;
        self.context
            .render_clear(
                request.logical_size,
                request.scale,
                [
                    f64::from(color.red) / 255.0,
                    f64::from(color.green) / 255.0,
                    f64::from(color.blue) / 255.0,
                    f64::from(color.alpha) / 255.0,
                ],
                self.options.max_target_bytes(),
            )
            .await
    }
}

#[derive(Debug)]
pub enum GpuRenderError {
    Initialize(GpuInitError),
    Render(anyhow::Error),
}

impl fmt::Display for GpuRenderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Initialize(error) => error.fmt(formatter),
            Self::Render(error) => write!(formatter, "headless GPU render failed: {error:#}"),
        }
    }
}

impl std::error::Error for GpuRenderError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Initialize(error) => Some(error),
            Self::Render(error) => Some(error.as_ref()),
        }
    }
}

/// Initializes the single GPU backend and publishes only its bounded,
/// single-owner service handle.
pub async fn start_renderer_service(
    options: GpuRendererOptions,
    queue_capacity: usize,
) -> Result<RendererService<GpuRenderError>, GpuRenderError> {
    RendererService::start::<GpuRenderer>(options, queue_capacity).await
}

impl RendererBackend for GpuRenderer {
    type Startup = GpuRendererOptions;
    type Error = GpuRenderError;

    async fn initialize(options: Self::Startup) -> Result<Self, Self::Error> {
        Self::new(options).await.map_err(GpuRenderError::Initialize)
    }

    async fn render(&mut self, request: RenderRequest) -> Result<Vec<u8>, Self::Error> {
        self.render_clear(request)
            .await
            .map(|image| image.png)
            .map_err(GpuRenderError::Render)
    }
}
