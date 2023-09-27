use anyhow::Result;
use async_trait::async_trait;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use tokio::sync::{mpsc, oneshot, Notify};
use worker::*;

#[macro_use]
extern crate anyhow;

mod worker;

/// The worker job trait. A user of the library need to implement
/// the "work" that the workers can do by implementing this trait
#[async_trait]
pub trait Work {
    type Input: Send + 'static;
    type Output: Send + 'static;
    async fn run(&self, input: Self::Input) -> Self::Output;
}

#[async_trait]
impl<W> Work for Arc<W>
where
    W: Work + Send + Sync, // notice that Work here is not itself a Clone. that is covered by Arc
{
    type Input = W::Input;
    type Output = W::Output;
    async fn run(&self, job: Self::Input) -> Self::Output {
        self.as_ref().run(job).await
    }
}

type Job<W> = oneshot::Sender<(
    <W as Work>::Input,
    Option<oneshot::Sender<<W as Work>::Output>>,
)>;

/// A pool of workers that can be configured to do certain "work". and
/// a fixed number of workers.
pub struct WorkerPool<W>
where
    W: Work,
{
    receiver: mpsc::Receiver<Job<W>>,
    notify: Arc<Notify>,
    size: Arc<AtomicUsize>,
}

impl<W> WorkerPool<W>
where
    W: Work + Send + Sync + Clone + 'static,
{
    /// create a new workers pool
    pub fn new(work: W, size: usize) -> WorkerPool<W> {
        assert!(size > 0, "pool cannot be of size 0");

        let (sender, receiver) = mpsc::channel(1);

        let notify = Arc::new(Notify::new());

        let count = Arc::new(AtomicUsize::new(size));
        for _ in 0..size {
            Worker::new(
                work.clone(),
                sender.clone(),
                Arc::clone(&notify),
                Arc::clone(&count),
            )
            .run();
        }

        WorkerPool {
            receiver,
            notify,
            size: count,
        }
    }

    /// get the first free worker. returns a worker handler
    pub async fn get(&mut self) -> WorkerHandle<W> {
        let sender = self.receiver.recv().await.unwrap();
        WorkerHandle { sender }
    }

    /// close make sure all workers exited before returning
    /// it does not cancel current worker job but instead waits
    /// for it to finish
    pub async fn close(mut self) {
        self.receiver.close();
        while self.size.load(Ordering::Relaxed) != 0 {
            self.notify.notified().await;
        }
    }
}

/// WorkerHandle is used to interact with a worker
pub struct WorkerHandle<W: Work> {
    pub sender: Job<W>,
}

impl<W> WorkerHandle<W>
where
    W: Work,
{
    /// send a job to worker, does not wait for results
    pub fn send(self, job: W::Input) -> Result<()> {
        if self.sender.send((job, None)).is_err() {
            bail!("failed to queue job");
        }

        Ok(())
    }

    /// sends a job to worker and waits for result
    pub async fn run(self, job: W::Input) -> Result<W::Output> {
        let (sender, receiver) = oneshot::channel();
        if self.sender.send((job, Some(sender))).is_err() {
            bail!("failed to queue job");
        }

        Ok(receiver.await?)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::{Work, WorkerPool};
    use async_trait::async_trait;
    use tokio::sync::Mutex;

    #[derive(Clone)]
    struct Adder {
        pub inc_val: u64,
    }

    #[async_trait]
    impl Work for Adder {
        type Input = Arc<Mutex<u64>>;
        type Output = ();
        async fn run(&self, job: Self::Input) {
            let mut var = job.lock().await;
            *var += self.inc_val;
        }
    }

    #[derive(Clone)]
    struct Multiplier;

    #[async_trait]
    impl Work for Multiplier {
        type Input = (f64, f64);
        type Output = f64;
        async fn run(&self, input: Self::Input) -> Self::Output {
            input.0 * input.1
        }
    }

    #[tokio::test]
    async fn test_workerpool() {
        let var = Arc::new(Mutex::new(0_u64));
        let adder = Adder { inc_val: 1_u64 };
        let mut pool = WorkerPool::<Adder>::new(adder, 100);

        for _ in 0..2000 {
            let worker = pool.get().await;
            let _ = worker.send(Arc::clone(&var)).unwrap();
        }

        pool.close().await;
        let var = *var.lock().await;
        assert_eq!(var, 2000);
    }

    #[tokio::test]
    async fn test_workerpool_ret() {
        let mult = Multiplier;
        let mut pool = WorkerPool::new(mult, 1);

        let worker = pool.get().await;
        let out = worker.run((10.0, 20.0)).await.unwrap();

        assert_eq!(out, 10.0 * 20.0);
    }
}
