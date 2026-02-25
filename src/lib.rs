use anyhow::{bail, Result};
use std::future::Future;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use tokio::sync::{mpsc, oneshot, Notify};
use worker::*;

mod worker;

/// The worker job trait. Implement this to define what work the pool performs.
///
/// Each worker in the pool receives its own [`Clone`] of the implementor.
/// The `&mut self` parameter refers to that worker's private clone, so
/// mutations are local to the individual worker — not shared across the pool.
/// Use this for per-worker caches, counters, or connections. For shared state,
/// pass it through [`Input`](Work::Input) (e.g. wrapped in `Arc<Mutex<T>>`).
pub trait Work: Send + Sync + Clone + 'static {
    type Input: Send + 'static;
    type Output: Send + 'static;
    fn run(&mut self, input: Self::Input) -> impl Future<Output = Self::Output> + Send;
}

/// The payload sent to a worker: the input and an optional channel to return the result.
type Task<W> = (<W as Work>::Input, Option<oneshot::Sender<<W as Work>::Output>>);

/// A slot offered by an idle worker, through which the pool can send it a task.
type WorkerSlot<W> = oneshot::Sender<Task<W>>;

/// A pool of workers that can be configured to do certain "work" with
/// a fixed number of workers.
///
/// Call [`close()`](WorkerPool::close) for a graceful async shutdown that
/// waits for all in-flight work to complete.
///
/// If the pool is dropped without `close()`, workers are signalled to stop
/// (they will finish their current job) but the drop does not block waiting
/// for them. Use `close()` when you need deterministic cleanup.
pub struct WorkerPool<W>
where
    W: Work,
{
    receiver: Option<mpsc::Receiver<WorkerSlot<W>>>,
    notify: Arc<Notify>,
    size: Arc<AtomicUsize>,
}

impl<W> WorkerPool<W>
where
    W: Work,
{
    /// create a new workers pool
    pub fn new(work: W, size: usize) -> WorkerPool<W> {
        assert!(size > 0, "pool cannot be of size 0");

        let (sender, receiver) = mpsc::channel(size);

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
            receiver: Some(receiver),
            notify,
            size: count,
        }
    }

    /// Get the first free worker. Returns a worker handle.
    /// Returns None if all workers have exited (e.g. due to panic or shutdown)
    /// or if the pool has been drained.
    pub async fn get(&mut self) -> Option<WorkerHandle<W>> {
        let sender = self.receiver.as_mut()?.recv().await?;
        Some(WorkerHandle { sender })
    }

    /// Returns the number of workers still alive (idle or busy).
    pub fn worker_count(&self) -> usize {
        self.size.load(Ordering::Acquire)
    }

    /// Returns true if the pool has been drained (no longer accepting new work).
    pub fn is_drained(&self) -> bool {
        self.receiver.is_none()
    }

    /// Stop accepting new work. Workers will finish their current job then exit.
    /// After draining, [`get()`](WorkerPool::get) returns `None`.
    /// Call [`wait()`](WorkerPool::wait) afterwards to wait for all workers to exit.
    pub fn drain(&mut self) {
        if let Some(ref mut rx) = self.receiver {
            // Close the channel so workers' send() calls fail.
            rx.close();
            // Drain buffered WorkerSlots (oneshot::Senders). Without this,
            // the channel's internal buffer holds onto them (it's Arc-shared
            // with the workers' Sender clones), and workers blocked on the
            // corresponding oneshot::Receiver never wake up — circular deadlock.
            while rx.try_recv().is_ok() {}
        }
        self.receiver.take();
    }

    /// Wait for all workers to exit. Typically called after [`drain()`](WorkerPool::drain).
    /// Returns immediately if all workers have already exited.
    pub async fn wait(&self) {
        loop {
            // Register interest BEFORE checking the count. This closes the
            // race where workers notify between our load() and the await:
            // since we're already registered, their notify_one() wakes us
            // (or stores a permit that enable() consumes on the next loop).
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            if self.size.load(Ordering::Acquire) == 0 {
                break;
            }

            notified.await;
        }
    }

    /// Graceful async shutdown. Equivalent to calling [`drain()`](WorkerPool::drain)
    /// followed by [`wait()`](WorkerPool::wait). Consumes the pool.
    pub async fn close(mut self) {
        self.drain();
        self.wait().await;
    }
}

impl<W> Drop for WorkerPool<W>
where
    W: Work,
{
    fn drop(&mut self) {
        // Drain the channel to unblock workers (see drain() for details).
        // We intentionally do not block waiting for workers to exit —
        // use close() for deterministic shutdown.
        self.drain();

        if self.size.load(Ordering::Acquire) != 0 {
            log::warn!(
                "WorkerPool dropped without close(), {} workers still running",
                self.size.load(Ordering::Acquire)
            );
        }
    }
}

/// WorkerHandle is used to interact with a worker
pub struct WorkerHandle<W: Work> {
    sender: WorkerSlot<W>,
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
    use std::future::Future;
    use std::sync::Arc;

    use super::{Work, WorkerPool};
    use tokio::sync::{Barrier, Mutex};

    // ---------------------------------------------------------------
    // Test Work implementations
    // ---------------------------------------------------------------

    #[derive(Clone)]
    struct Adder {
        pub inc_val: u64,
    }

    impl Work for Adder {
        type Input = Arc<Mutex<u64>>;
        type Output = ();
        fn run(&mut self, job: Self::Input) -> impl Future<Output = ()> + Send {
            let inc_val = self.inc_val;
            async move {
                let mut var = job.lock().await;
                *var += inc_val;
            }
        }
    }

    #[derive(Clone)]
    struct Multiplier;

    impl Work for Multiplier {
        type Input = (f64, f64);
        type Output = f64;
        fn run(&mut self, input: Self::Input) -> impl Future<Output = f64> + Send {
            async move { input.0 * input.1 }
        }
    }

    /// Worker that sleeps for a given duration, useful for timing-sensitive tests.
    #[derive(Clone)]
    struct SlowWorker;

    impl Work for SlowWorker {
        type Input = std::time::Duration;
        type Output = ();
        fn run(&mut self, dur: Self::Input) -> impl Future<Output = ()> + Send {
            async move { tokio::time::sleep(dur).await }
        }
    }

    /// Worker that blocks on a barrier, so we can guarantee all N workers
    /// are busy simultaneously.
    #[derive(Clone)]
    struct BarrierWorker {
        barrier: Arc<Barrier>,
    }

    impl Work for BarrierWorker {
        type Input = ();
        type Output = ();
        fn run(&mut self, _: Self::Input) -> impl Future<Output = ()> + Send {
            let barrier = Arc::clone(&self.barrier);
            async move {
                barrier.wait().await;
            }
        }
    }

    /// Worker that panics on a specific input.
    #[derive(Clone)]
    struct PanicWorker;

    impl Work for PanicWorker {
        type Input = bool;
        type Output = ();
        fn run(&mut self, should_panic: Self::Input) -> impl Future<Output = ()> + Send {
            async move {
                if should_panic {
                    panic!("intentional test panic");
                }
            }
        }
    }

    // ---------------------------------------------------------------
    // Basic functionality
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn fire_and_forget_correctness() {
        let var = Arc::new(Mutex::new(0_u64));
        let adder = Adder { inc_val: 1_u64 };
        let mut pool = WorkerPool::<Adder>::new(adder, 100);

        for _ in 0..2000 {
            let worker = pool.get().await.expect("worker should be available");
            worker.send(Arc::clone(&var)).unwrap();
        }

        pool.close().await;
        assert_eq!(*var.lock().await, 2000);
    }

    #[tokio::test]
    async fn run_returns_result() {
        let mult = Multiplier;
        let mut pool = WorkerPool::new(mult, 1);

        let worker = pool.get().await.expect("worker should be available");
        let out = worker.run((10.0, 20.0)).await.unwrap();
        assert_eq!(out, 200.0);
    }

    #[tokio::test]
    async fn handle_drop_without_send_recovers() {
        let mult = Multiplier;
        let mut pool = WorkerPool::new(mult, 1);

        let worker = pool.get().await.expect("worker should be available");
        drop(worker);

        // Worker should re-register after its handle was dropped
        let worker = pool.get().await.expect("worker should recover");
        let out = worker.run((10.0, 20.0)).await.unwrap();
        assert_eq!(out, 200.0);
    }

    // ---------------------------------------------------------------
    // Pool size = 1 (degenerate case, must be strictly sequential)
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn pool_size_1_sequential_correctness() {
        let var = Arc::new(Mutex::new(0_u64));
        let adder = Adder { inc_val: 1_u64 };
        let mut pool = WorkerPool::new(adder, 1);

        // Each get() must wait for the single worker to finish its previous job
        for _ in 0..500 {
            let worker = pool.get().await.expect("worker should be available");
            worker.send(Arc::clone(&var)).unwrap();
        }

        pool.close().await;
        assert_eq!(*var.lock().await, 500);
    }

    #[tokio::test]
    async fn pool_size_1_alternating_send_and_run() {
        let var = Arc::new(Mutex::new(0_u64));
        let adder = Adder { inc_val: 1_u64 };
        let mut pool = WorkerPool::new(adder, 1);

        for i in 0..100 {
            let worker = pool.get().await.expect("worker should be available");
            if i % 2 == 0 {
                worker.send(Arc::clone(&var)).unwrap();
            } else {
                worker.run(Arc::clone(&var)).await.unwrap();
            }
        }

        pool.close().await;
        assert_eq!(*var.lock().await, 100);
    }

    #[tokio::test]
    async fn pool_size_1_repeated_drop_recovery() {
        let mult = Multiplier;
        let mut pool = WorkerPool::new(mult, 1);

        // Drop the handle 50 times in a row — worker must recover each time
        for _ in 0..50 {
            let worker = pool.get().await.expect("worker should recover");
            drop(worker);
        }

        // Still works after all those drops
        let worker = pool.get().await.expect("worker should be available");
        let out = worker.run((3.0, 7.0)).await.unwrap();
        assert_eq!(out, 21.0);
    }

    // ---------------------------------------------------------------
    // All workers busy simultaneously (saturation)
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn all_workers_busy_then_release() {
        let n = 8;
        // Barrier needs n+1: n workers + 1 for the test to release them
        let barrier = Arc::new(Barrier::new(n + 1));
        let bw = BarrierWorker {
            barrier: Arc::clone(&barrier),
        };
        let mut pool = WorkerPool::new(bw, n);

        // Dispatch a job to every worker — they all block on the barrier
        for _ in 0..n {
            let worker = pool.get().await.expect("worker should be available");
            worker.send(()).unwrap();
        }

        // All n workers are now blocked. Release them.
        barrier.wait().await;

        // Workers should become available again
        for _ in 0..n {
            let worker = pool.get().await.expect("worker should be available after barrier");
            worker.send(()).unwrap();
        }

        // Release the second round
        // Need a new barrier for the second round - but our workers still hold
        // the old barrier. Actually the second send() uses the same barrier worker
        // clone, so they'll block on the same barrier. We need n workers to arrive
        // again. But we already had n+1 waits (n workers + 1 test), so the barrier
        // has reset. The second batch of n workers will block, and we wait once more.
        barrier.wait().await;

        pool.close().await;
    }

    // ---------------------------------------------------------------
    // Concurrent get() callers (multiple tasks fighting for workers)
    // ---------------------------------------------------------------

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_get_callers() {
        let var = Arc::new(Mutex::new(0_u64));
        let adder = Adder { inc_val: 1_u64 };
        let pool = Arc::new(Mutex::new(WorkerPool::new(adder, 4)));

        let mut handles = Vec::new();
        for _ in 0..100 {
            let pool = Arc::clone(&pool);
            let var = Arc::clone(&var);
            handles.push(tokio::spawn(async move {
                let mut pool = pool.lock().await;
                let worker = pool.get().await.expect("worker should be available");
                drop(pool); // release pool lock before sending work
                worker.send(var).unwrap();
            }));
        }

        for h in handles {
            h.await.unwrap();
        }

        // All spawned tasks are done, drain and wait via the shared reference
        pool.lock().await.drain();
        pool.lock().await.wait().await;
        assert_eq!(*var.lock().await, 100);
    }

    // ---------------------------------------------------------------
    // Results under concurrency — every job gets its correct result
    // ---------------------------------------------------------------

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn parallel_results_correctness() {
        let mult = Multiplier;
        let pool = Arc::new(Mutex::new(WorkerPool::new(mult, 8)));

        let mut handles = Vec::new();
        for i in 0..200 {
            let pool = Arc::clone(&pool);
            let a = i as f64;
            let b = (i + 1) as f64;
            handles.push(tokio::spawn(async move {
                let mut pool = pool.lock().await;
                let worker = pool.get().await.expect("worker should be available");
                drop(pool);
                let result = worker.run((a, b)).await.unwrap();
                assert_eq!(result, a * b, "wrong result for ({a}, {b})");
            }));
        }

        for h in handles {
            h.await.unwrap();
        }
    }

    // ---------------------------------------------------------------
    // Worker panic handling
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn panic_worker_pool_survives() {
        let mut pool = WorkerPool::new(PanicWorker, 4);

        // Kill one worker
        let worker = pool.get().await.expect("worker should be available");
        worker.send(true).unwrap(); // this worker will panic

        // Give the panic time to propagate
        tokio::task::yield_now().await;

        // Pool should still function with remaining workers
        // (one worker is dead, 3 remain)
        let worker = pool.get().await.expect("surviving workers should be available");
        worker.send(false).unwrap();

        pool.close().await;
    }

    #[tokio::test]
    async fn panic_all_workers_get_returns_none() {
        let mut pool = WorkerPool::new(PanicWorker, 2);

        // Kill both workers
        for _ in 0..2 {
            let worker = pool.get().await.expect("worker should be available");
            worker.send(true).unwrap();
        }

        // Give panics time to propagate
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;

        // get() should return None — all workers are dead
        assert!(pool.get().await.is_none());
        assert_eq!(pool.worker_count(), 0);
    }

    #[tokio::test]
    async fn panic_worker_close_does_not_hang() {
        let mut pool = WorkerPool::new(PanicWorker, 3);

        // Kill 2 out of 3
        for _ in 0..2 {
            let worker = pool.get().await.expect("worker should be available");
            worker.send(true).unwrap();
        }

        // close() must not hang even though 2 workers panicked
        tokio::time::timeout(std::time::Duration::from_secs(2), pool.close())
            .await
            .expect("close() should not hang after worker panics");
    }

    #[tokio::test]
    async fn panic_worker_count_decrements() {
        let mut pool = WorkerPool::new(PanicWorker, 5);

        let worker = pool.get().await.expect("worker should be available");
        worker.send(true).unwrap();

        // Give the panic time to decrement the counter
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        assert_eq!(pool.worker_count(), 4);
        pool.close().await;
    }

    // ---------------------------------------------------------------
    // Drain / wait / close lifecycle
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn drain_then_wait() {
        let var = Arc::new(Mutex::new(0_u64));
        let adder = Adder { inc_val: 1_u64 };
        let mut pool = WorkerPool::<Adder>::new(adder, 10);

        for _ in 0..100 {
            let worker = pool.get().await.expect("worker should be available");
            worker.send(Arc::clone(&var)).unwrap();
        }

        assert!(!pool.is_drained());
        pool.drain();
        assert!(pool.is_drained());
        assert!(pool.get().await.is_none());

        pool.wait().await;
        assert_eq!(pool.worker_count(), 0);
        assert_eq!(*var.lock().await, 100);
    }

    #[tokio::test]
    async fn close_idle_pool() {
        // No work submitted — close should still complete promptly
        let mult = Multiplier;
        let pool = WorkerPool::new(mult, 10);

        tokio::time::timeout(std::time::Duration::from_secs(2), pool.close())
            .await
            .expect("closing an idle pool should not hang");
    }

    #[tokio::test]
    async fn close_immediately_after_creation() {
        let mult = Multiplier;
        let pool = WorkerPool::new(mult, 50);

        tokio::time::timeout(std::time::Duration::from_secs(2), pool.close())
            .await
            .expect("immediate close should not hang");
    }

    #[tokio::test]
    async fn double_drain_is_idempotent() {
        let mult = Multiplier;
        let mut pool = WorkerPool::new(mult, 5);
        pool.drain();
        pool.drain(); // should not panic
        pool.wait().await;
        assert_eq!(pool.worker_count(), 0);
    }

    #[tokio::test]
    async fn worker_count_reflects_state() {
        let mult = Multiplier;
        let mut pool = WorkerPool::new(mult, 5);
        assert_eq!(pool.worker_count(), 5);

        pool.drain();
        pool.wait().await;
        assert_eq!(pool.worker_count(), 0);
    }

    // ---------------------------------------------------------------
    // Slow workers — drain while work is in-flight
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn drain_while_workers_are_busy() {
        let mut pool = WorkerPool::new(SlowWorker, 4);

        // Dispatch slow jobs to all 4 workers
        for _ in 0..4 {
            let worker = pool.get().await.expect("worker should be available");
            worker.send(std::time::Duration::from_millis(100)).unwrap();
        }

        // Drain immediately — workers are still sleeping
        pool.drain();

        // Wait should block until all slow jobs finish, then workers exit
        tokio::time::timeout(std::time::Duration::from_secs(2), pool.wait())
            .await
            .expect("wait after drain should complete once jobs finish");

        assert_eq!(pool.worker_count(), 0);
    }

    // ---------------------------------------------------------------
    // Large pool sizes (stress the buffer=size change)
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn large_pool_correctness() {
        let var = Arc::new(Mutex::new(0_u64));
        let adder = Adder { inc_val: 1_u64 };
        let mut pool = WorkerPool::new(adder, 500);

        for _ in 0..5000 {
            let worker = pool.get().await.expect("worker should be available");
            worker.send(Arc::clone(&var)).unwrap();
        }

        pool.close().await;
        assert_eq!(*var.lock().await, 5000);
    }

    #[tokio::test]
    async fn large_pool_close_immediately() {
        let mult = Multiplier;
        let pool = WorkerPool::new(mult, 500);

        tokio::time::timeout(std::time::Duration::from_secs(5), pool.close())
            .await
            .expect("closing large idle pool should not hang");
    }

    // ---------------------------------------------------------------
    // Mixed fire-and-forget + result jobs on same pool
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn mixed_send_and_run() {
        let var = Arc::new(Mutex::new(0_u64));
        let adder = Adder { inc_val: 1_u64 };
        let mut pool = WorkerPool::new(adder, 8);

        for i in 0..200 {
            let worker = pool.get().await.expect("worker should be available");
            if i % 3 == 0 {
                // run() — wait for result
                worker.run(Arc::clone(&var)).await.unwrap();
            } else {
                // send() — fire and forget
                worker.send(Arc::clone(&var)).unwrap();
            }
        }

        pool.close().await;
        assert_eq!(*var.lock().await, 200);
    }

    // ---------------------------------------------------------------
    // Edge case: pool of size 1, many handle drops interleaved with real work
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn size_1_drop_interleave_stress() {
        let var = Arc::new(Mutex::new(0_u64));
        let adder = Adder { inc_val: 1_u64 };
        let mut pool = WorkerPool::new(adder, 1);

        for i in 0..300 {
            let worker = pool.get().await.expect("worker should be available");
            if i % 5 == 0 {
                // Drop without sending — worker must recover
                drop(worker);
            } else {
                worker.send(Arc::clone(&var)).unwrap();
            }
        }

        pool.close().await;
        // 300 iterations, every 5th is dropped = 60 drops, 240 actual jobs
        assert_eq!(*var.lock().await, 240);
    }

    // ---------------------------------------------------------------
    // Verify per-worker clone isolation (#8 semantics)
    // ---------------------------------------------------------------

    #[derive(Clone)]
    struct CountingWorker {
        call_count: Arc<Mutex<Vec<usize>>>,
    }

    impl Work for CountingWorker {
        type Input = ();
        type Output = ();
        fn run(&mut self, _: Self::Input) -> impl Future<Output = ()> + Send {
            let log = Arc::clone(&self.call_count);
            async move {
                // Each clone will push to the shared log
                let mut v = log.lock().await;
                v.push(1);
            }
        }
    }

    #[tokio::test]
    async fn per_worker_clone_isolation() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let cw = CountingWorker {
            call_count: Arc::clone(&log),
        };
        let mut pool = WorkerPool::new(cw, 4);

        for _ in 0..100 {
            let worker = pool.get().await.expect("worker should be available");
            worker.send(()).unwrap();
        }

        pool.close().await;
        assert_eq!(log.lock().await.len(), 100);
    }
}
