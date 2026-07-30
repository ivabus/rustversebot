//! Single-owner asynchronous renderer service.
//!
//! The service initializes one backend, moves it into a dedicated task, and
//! serializes all rendering through a bounded request queue. Keeping the
//! backend in that task lets stateful GPU resources (device, queue, pipelines,
//! atlases, and glyph caches) live for the full service lifetime.

use std::error::Error;
use std::fmt;
use std::future::Future;

use tokio::sync::{mpsc, oneshot};

use crate::renderer::RenderScale;
use crate::scene::{LogicalSize, Scene, ShapeNode};

/// A solid RGBA color with straight, eight-bit channels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SolidColor {
    pub red: u8,
    pub green: u8,
    pub blue: u8,
    pub alpha: u8,
}

impl SolidColor {
    pub const fn rgba(red: u8, green: u8, blue: u8, alpha: u8) -> Self {
        Self {
            red,
            green,
            blue,
            alpha,
        }
    }
}

/// Backend-neutral input for a headless render.
#[derive(Clone, Debug, PartialEq)]
pub struct RenderRequest {
    scene: Scene<ShapeNode>,
    scale: RenderScale,
    clear_color: SolidColor,
}

impl RenderRequest {
    /// Creates an empty scene, preserving the Phase 2 clear-pass ergonomics.
    pub fn clear(logical_size: LogicalSize, scale: RenderScale, clear_color: SolidColor) -> Self {
        Self::scene(Scene::new(logical_size), scale, clear_color)
    }

    pub fn scene(scene: Scene<ShapeNode>, scale: RenderScale, clear_color: SolidColor) -> Self {
        Self {
            scene,
            scale,
            clear_color,
        }
    }

    pub fn scene_ref(&self) -> &Scene<ShapeNode> {
        &self.scene
    }

    pub fn scale(&self) -> RenderScale {
        self.scale
    }

    pub fn clear_color(&self) -> SolidColor {
        self.clear_color
    }

    pub fn into_parts(self) -> (Scene<ShapeNode>, RenderScale, SolidColor) {
        (self.scene, self.scale, self.clear_color)
    }
}

/// Backend contract for the service.
///
/// `initialize` is called exactly once, before the service handle is returned.
/// Its input is the hook for reusable startup resources such as an adapter,
/// asset manifest, or predecoded assets. `render` is only ever called by the
/// owner task, with the same mutable backend instance.
pub trait RendererBackend: Send + Sized + 'static {
    type Startup: Send + 'static;
    type Error: Send + 'static;

    fn initialize(startup: Self::Startup)
    -> impl Future<Output = Result<Self, Self::Error>> + Send;

    fn render(
        &mut self,
        request: RenderRequest,
    ) -> impl Future<Output = Result<Vec<u8>, Self::Error>> + Send;
}

/// Failure returned while submitting or receiving a render.
#[derive(Debug)]
pub enum RenderServiceError<BackendError> {
    /// The backend rejected this request. Later requests are still processed.
    Backend(BackendError),
    /// The renderer task no longer accepts requests.
    Unavailable,
    /// The renderer task ended before sending this request's response.
    ResponseDropped,
}

impl<BackendError: fmt::Display> fmt::Display for RenderServiceError<BackendError> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Backend(error) => write!(formatter, "renderer backend failed: {error}"),
            Self::Unavailable => formatter.write_str("renderer service is unavailable"),
            Self::ResponseDropped => formatter.write_str("renderer service dropped the response"),
        }
    }
}

impl<BackendError> Error for RenderServiceError<BackendError>
where
    BackendError: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Backend(error) => Some(error),
            Self::Unavailable | Self::ResponseDropped => None,
        }
    }
}

/// Immediate submission failure from [`RendererService::try_submit`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrySubmitError {
    QueueFull,
    Unavailable,
}

impl fmt::Display for TrySubmitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::QueueFull => formatter.write_str("renderer request queue is full"),
            Self::Unavailable => formatter.write_str("renderer service is unavailable"),
        }
    }
}

impl Error for TrySubmitError {}

/// A response ticket returned after a request enters the queue.
pub struct RenderResponse<BackendError> {
    receiver: oneshot::Receiver<Result<Vec<u8>, BackendError>>,
}

impl<BackendError> RenderResponse<BackendError> {
    pub async fn receive(self) -> Result<Vec<u8>, RenderServiceError<BackendError>> {
        self.receiver
            .await
            .map_err(|_| RenderServiceError::ResponseDropped)?
            .map_err(RenderServiceError::Backend)
    }
}

struct Envelope<BackendError> {
    request: RenderRequest,
    response: oneshot::Sender<Result<Vec<u8>, BackendError>>,
}

/// Cloneable producer handle for one long-lived renderer backend.
pub struct RendererService<BackendError> {
    sender: mpsc::Sender<Envelope<BackendError>>,
}

impl<BackendError> Clone for RendererService<BackendError> {
    fn clone(&self) -> Self {
        Self {
            sender: self.sender.clone(),
        }
    }
}

impl<BackendError: Send + 'static> RendererService<BackendError> {
    /// Initializes `Backend` once and starts its single owner task.
    ///
    /// A capacity of zero is rejected because Tokio's bounded channel requires
    /// at least one buffered slot.
    pub async fn start<Backend>(
        startup: Backend::Startup,
        queue_capacity: usize,
    ) -> Result<Self, Backend::Error>
    where
        Backend: RendererBackend<Error = BackendError>,
    {
        assert!(
            queue_capacity > 0,
            "renderer service queue capacity must be greater than zero"
        );

        let backend = Backend::initialize(startup).await?;
        let (sender, receiver) = mpsc::channel(queue_capacity);
        let runtime = tokio::runtime::Handle::current();
        std::thread::Builder::new()
            .name("rustverse-svg-renderer".to_owned())
            .spawn(move || runtime.block_on(run_backend(backend, receiver)))
            .expect("failed to start renderer owner thread");
        Ok(Self { sender })
    }

    /// Waits for bounded queue capacity, then waits for the rendered bytes.
    pub async fn render(
        &self,
        request: RenderRequest,
    ) -> Result<Vec<u8>, RenderServiceError<BackendError>> {
        let response = self.submit(request).await?;
        response.receive().await
    }

    /// Waits until the request can enter the bounded queue.
    pub async fn submit(
        &self,
        request: RenderRequest,
    ) -> Result<RenderResponse<BackendError>, RenderServiceError<BackendError>> {
        let (response, receiver) = oneshot::channel();
        self.sender
            .send(Envelope { request, response })
            .await
            .map_err(|_| RenderServiceError::Unavailable)?;
        Ok(RenderResponse { receiver })
    }

    /// Attempts to enqueue immediately, exposing bounded backpressure.
    pub fn try_submit(
        &self,
        request: RenderRequest,
    ) -> Result<RenderResponse<BackendError>, TrySubmitError> {
        let (response, receiver) = oneshot::channel();
        self.sender
            .try_send(Envelope { request, response })
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => TrySubmitError::QueueFull,
                mpsc::error::TrySendError::Closed(_) => TrySubmitError::Unavailable,
            })?;
        Ok(RenderResponse { receiver })
    }

    pub fn available_queue_capacity(&self) -> usize {
        self.sender.capacity()
    }

    pub fn queue_capacity(&self) -> usize {
        self.sender.max_capacity()
    }
}

async fn run_backend<Backend>(
    mut backend: Backend,
    mut receiver: mpsc::Receiver<Envelope<Backend::Error>>,
) where
    Backend: RendererBackend,
{
    while let Some(envelope) = receiver.recv().await {
        // Do not spend GPU/encoding time on work whose caller went away while
        // it was waiting behind an earlier request.
        if envelope.response.is_closed() {
            continue;
        }

        let result = backend.render(envelope.request).await;
        // A caller can also disappear during rendering. Discarding the result
        // leaves the backend healthy for the next request.
        let _ = envelope.response.send(result);
    }
}
