use anyhow::Result;
use std::{future::Future, pin::Pin};
use tokio::{
    sync::{mpsc, watch},
    task::JoinSet,
};

pub(crate) type Job = Pin<Box<dyn Future<Output = Result<()>> + Send>>;
const ACTIVE: usize = 4;

pub(crate) fn channel() -> (mpsc::Sender<Job>, mpsc::Receiver<Job>) {
    mpsc::channel(8)
}

pub(crate) async fn run(mut queue: mpsc::Receiver<Job>, mut shutdown: watch::Receiver<bool>) {
    let mut active = JoinSet::new();
    loop {
        tokio::select! {
            biased;
            _ = shutdown.wait_for(|stop|*stop) => break,
            Some(result) = active.join_next(), if !active.is_empty() => match result {
                Ok(Ok(())) => {},
                Ok(Err(e)) => log::warn!("AI job failed: {e:#}"),
                Err(e) => log::error!("AI job stopped: {e}"),
            },
            job = queue.recv(), if active.len()<ACTIVE => match job {
                Some(job) => { active.spawn(job); }
                None => break,
            },
        }
    }
    active.shutdown().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[tokio::test]
    async fn active_and_waiting_jobs_are_bounded_and_stop_on_shutdown() {
        let (queue, rx) = channel();
        let (stop, shutdown) = watch::channel(false);
        let worker = tokio::spawn(run(rx, shutdown));
        let (started, mut starts) = mpsc::unbounded_channel();
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let job = || -> Job {
            let started = started.clone();
            let gate = gate.clone();
            Box::pin(async move {
                started.send(())?;
                gate.acquire().await?.forget();
                Ok(())
            })
        };
        for _ in 0..ACTIVE {
            queue.send(job()).await.unwrap();
            starts.recv().await.unwrap();
        }
        for _ in 0..queue.max_capacity() {
            assert!(queue.try_send(job()).is_ok());
        }
        assert!(matches!(
            queue.try_reserve(),
            Err(mpsc::error::TrySendError::Full(_))
        ));
        gate.add_permits(1);
        starts.recv().await.unwrap();
        assert!(queue.try_reserve().is_ok());
        stop.send(true).unwrap();
        worker.await.unwrap();
        assert!(queue.is_closed());
        assert_eq!(Arc::strong_count(&gate), 1);
    }
}
