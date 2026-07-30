use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use rustverse_svg::RenderScale;
use rustverse_svg::renderer_service::{
    RenderRequest, RendererBackend, RendererService, SolidColor, TrySubmitError,
};
use rustverse_svg::scene::LogicalSize;
use tokio::sync::{Semaphore, mpsc};

fn request(red: u8) -> RenderRequest {
    RenderRequest::clear(
        LogicalSize {
            width: 16.0,
            height: 9.0,
        },
        RenderScale::ONE,
        SolidColor::rgba(red, 2, 3, 255),
    )
}

struct CountingStartup {
    initializations: Arc<AtomicUsize>,
    renders: Arc<AtomicUsize>,
}

struct CountingBackend {
    renders: Arc<AtomicUsize>,
}

impl RendererBackend for CountingBackend {
    type Startup = CountingStartup;
    type Error = &'static str;

    async fn initialize(startup: Self::Startup) -> Result<Self, Self::Error> {
        startup.initializations.fetch_add(1, Ordering::SeqCst);
        Ok(Self {
            renders: startup.renders,
        })
    }

    async fn render(&mut self, request: RenderRequest) -> Result<Vec<u8>, Self::Error> {
        self.renders.fetch_add(1, Ordering::SeqCst);
        if request.clear_color().red == 0 {
            Err("synthetic render failure")
        } else {
            Ok(vec![
                request.clear_color().red,
                request.scene_ref().logical_size.width as u8,
                request.scale().factor() as u8,
            ])
        }
    }
}

#[tokio::test]
async fn startup_and_backend_are_reused_across_requests() {
    let initializations = Arc::new(AtomicUsize::new(0));
    let renders = Arc::new(AtomicUsize::new(0));
    let service = RendererService::start::<CountingBackend>(
        CountingStartup {
            initializations: initializations.clone(),
            renders: renders.clone(),
        },
        2,
    )
    .await
    .unwrap();

    assert_eq!(service.render(request(10)).await.unwrap(), [10, 16, 1]);
    assert_eq!(service.render(request(20)).await.unwrap(), [20, 16, 1]);
    assert_eq!(initializations.load(Ordering::SeqCst), 1);
    assert_eq!(renders.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn a_request_error_does_not_poison_the_backend() {
    let service = RendererService::start::<CountingBackend>(
        CountingStartup {
            initializations: Arc::new(AtomicUsize::new(0)),
            renders: Arc::new(AtomicUsize::new(0)),
        },
        1,
    )
    .await
    .unwrap();

    assert!(service.render(request(0)).await.is_err());
    assert_eq!(service.render(request(7)).await.unwrap(), [7, 16, 1]);
}

struct GatedStartup {
    entered: mpsc::UnboundedSender<u8>,
    release: Arc<Semaphore>,
}

struct GatedBackend {
    entered: mpsc::UnboundedSender<u8>,
    release: Arc<Semaphore>,
}

impl RendererBackend for GatedBackend {
    type Startup = GatedStartup;
    type Error = Infallible;

    async fn initialize(startup: Self::Startup) -> Result<Self, Self::Error> {
        Ok(Self {
            entered: startup.entered,
            release: startup.release,
        })
    }

    async fn render(&mut self, request: RenderRequest) -> Result<Vec<u8>, Self::Error> {
        let _ = self.entered.send(request.clear_color().red);
        self.release.acquire().await.unwrap().forget();
        Ok(vec![request.clear_color().red])
    }
}

#[tokio::test]
async fn bounded_queue_exposes_backpressure() {
    let (entered_tx, mut entered_rx) = mpsc::unbounded_channel();
    let release = Arc::new(Semaphore::new(0));
    let service = RendererService::start::<GatedBackend>(
        GatedStartup {
            entered: entered_tx,
            release: release.clone(),
        },
        1,
    )
    .await
    .unwrap();

    let first = service.try_submit(request(1)).unwrap();
    assert_eq!(entered_rx.recv().await.unwrap(), 1);
    let second = service.try_submit(request(2)).unwrap();
    assert!(matches!(
        service.try_submit(request(3)),
        Err(TrySubmitError::QueueFull)
    ));

    release.add_permits(2);
    assert_eq!(first.receive().await.unwrap(), [1]);
    assert_eq!(second.receive().await.unwrap(), [2]);
}

#[tokio::test]
async fn a_cancelled_queued_request_is_not_rendered() {
    let (entered_tx, mut entered_rx) = mpsc::unbounded_channel();
    let release = Arc::new(Semaphore::new(0));
    let service = RendererService::start::<GatedBackend>(
        GatedStartup {
            entered: entered_tx,
            release: release.clone(),
        },
        1,
    )
    .await
    .unwrap();

    let first = service.try_submit(request(1)).unwrap();
    assert_eq!(entered_rx.recv().await.unwrap(), 1);

    let cancelled = service.try_submit(request(2)).unwrap();
    drop(cancelled);

    release.add_permits(1);
    assert_eq!(first.receive().await.unwrap(), [1]);

    let third = service.submit(request(3)).await.unwrap();
    assert_eq!(entered_rx.recv().await.unwrap(), 3);
    release.add_permits(1);
    assert_eq!(third.receive().await.unwrap(), [3]);
}
