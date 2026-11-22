use anyhow::Result;
use futures_util::future::AbortHandle;
use std::{
    collections::VecDeque,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
};
use tokio::{
    sync::{OwnedSemaphorePermit, Semaphore, watch},
    task::JoinSet,
};

pub(crate) type Job = Pin<Box<dyn Future<Output = Result<()>> + Send>>;
const ACTIVE: usize = 4;
const WAITING: usize = 8;

struct Entry {
    id: Option<uuid::Uuid>,
    job: Job,
    _slot: OwnedSemaphorePermit,
}

struct Waiting {
    entries: Mutex<VecDeque<Entry>>,
    slots: Arc<Semaphore>,
}

#[derive(Clone)]
pub(crate) struct Sender {
    queue: Arc<Waiting>,
    wake: watch::Sender<()>,
}

pub(crate) struct Receiver {
    queue: Arc<Waiting>,
    wake: watch::Receiver<()>,
}

pub(crate) struct Permit<'a> {
    sender: &'a Sender,
    slot: OwnedSemaphorePermit,
}

pub(crate) fn channel() -> (Sender, Receiver) {
    let queue = Arc::new(Waiting {
        entries: Mutex::default(),
        slots: Arc::new(Semaphore::new(WAITING)),
    });
    let (wake, rx) = watch::channel(());
    (
        Sender {
            queue: queue.clone(),
            wake,
        },
        Receiver { queue, wake: rx },
    )
}

impl Sender {
    pub fn try_reserve(&self) -> Option<Permit<'_>> {
        Some(Permit {
            sender: self,
            slot: self.queue.slots.clone().try_acquire_owned().ok()?,
        })
    }

    pub fn remove(&self, id: uuid::Uuid) {
        self.queue
            .entries
            .lock()
            .unwrap()
            .retain(|entry| entry.id != Some(id));
    }
}

impl Permit<'_> {
    pub fn send(self, cancellation: Option<(uuid::Uuid, AbortHandle)>, job: Job) {
        let mut entries = self.sender.queue.entries.lock().unwrap();
        if self.sender.queue.slots.is_closed()
            || cancellation.as_ref().is_some_and(|(_, h)| h.is_aborted())
        {
            return;
        }
        entries.push_back(Entry {
            id: cancellation.map(|(id, _)| id),
            job,
            _slot: self.slot,
        });
        self.sender.wake.send_replace(());
    }
}

impl Receiver {
    pub fn try_recv(&self) -> Option<Job> {
        self.queue
            .entries
            .lock()
            .unwrap()
            .pop_front()
            .map(|entry| entry.job)
    }

    async fn recv(&mut self) -> Option<Job> {
        loop {
            if let Some(job) = self.try_recv() {
                return Some(job);
            }
            self.wake.changed().await.ok()?;
        }
    }
}

impl Drop for Receiver {
    fn drop(&mut self) {
        self.queue.slots.close();
        self.queue.entries.lock().unwrap().clear();
    }
}

pub(crate) async fn run(mut queue: Receiver, mut shutdown: watch::Receiver<bool>) {
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
    use tokio::sync::mpsc;

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
            queue.try_reserve().unwrap().send(None, job());
            starts.recv().await.unwrap();
        }
        for _ in 0..WAITING {
            queue.try_reserve().unwrap().send(None, job());
        }
        assert!(queue.try_reserve().is_none());
        gate.add_permits(1);
        starts.recv().await.unwrap();
        assert!(queue.try_reserve().is_some());
        stop.send(true).unwrap();
        worker.await.unwrap();
        assert!(queue.try_reserve().is_none());
        assert_eq!(Arc::strong_count(&gate), 1);
    }

    #[tokio::test]
    async fn cancelling_a_waiter_immediately_releases_its_slot_and_preserves_order() {
        let (queue, rx) = channel();
        let (sent, mut received) = mpsc::unbounded_channel();
        let job = |i| -> Job {
            let sent = sent.clone();
            Box::pin(async move {
                sent.send(i)?;
                Ok(())
            })
        };
        let id = uuid::Uuid::new_v4();
        let (abort, _) = AbortHandle::new_pair();
        for i in 0..WAITING {
            queue
                .try_reserve()
                .unwrap()
                .send((i == 1).then(|| (id, abort.clone())), job(i));
        }
        assert!(queue.try_reserve().is_none());
        abort.abort();
        queue.remove(id);
        // A stop racing with publication of a reserved job also wins.
        queue.try_reserve().unwrap().send(Some((id, abort)), job(1));
        queue.try_reserve().unwrap().send(None, job(WAITING));
        while let Some(job) = rx.try_recv() {
            job.await.unwrap();
        }
        for i in (0..=WAITING).filter(|&i| i != 1) {
            assert_eq!(received.try_recv().unwrap(), i);
        }
        assert!(received.try_recv().is_err());
    }
}
