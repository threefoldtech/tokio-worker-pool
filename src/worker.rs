use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use tokio::sync::{mpsc, oneshot, Notify};

use super::{Work, WorkerSlot};

/// Guard that decrements the worker count and notifies the pool on drop.
/// This ensures cleanup happens even if the worker task panics.
struct WorkerGuard {
    count: Arc<AtomicUsize>,
    notify: Arc<Notify>,
}

impl Drop for WorkerGuard {
    fn drop(&mut self) {
        self.count.fetch_sub(1, Ordering::Release);
        // notify_one() stores a permit if no waiter is registered, which
        // prevents a race between wait()'s load() and notified().await.
        // notify_waiters() does NOT store permits, so it must not be used here.
        self.notify.notify_one();
    }
}

pub struct Worker<W: Work> {
    work: W,
    sender: mpsc::Sender<WorkerSlot<W>>,
    notify: Arc<Notify>,
    count: Arc<AtomicUsize>,
}

impl<W> Worker<W>
where
    W: Work,
{
    pub fn new(
        work: W,
        sender: mpsc::Sender<WorkerSlot<W>>,
        notify: Arc<Notify>,
        count: Arc<AtomicUsize>,
    ) -> Self {
        Self {
            work,
            sender,
            notify,
            count,
        }
    }

    pub fn run(mut self) {
        tokio::spawn(async move {
            // Guard lives for the duration of the task. If the task panics,
            // the guard is dropped during unwinding, guaranteeing cleanup.
            let _guard = WorkerGuard {
                count: Arc::clone(&self.count),
                notify: Arc::clone(&self.notify),
            };

            loop {
                let (tx, rx) = oneshot::channel();

                if self.sender.send(tx).await.is_err() {
                    log::debug!("worker exiting");
                    break;
                }

                // Why select! is needed here:
                //
                // Normal path: the pool calls get() → receives `tx` from
                // the mpsc → sends a job through it → `rx` resolves here.
                //
                // Shutdown deadlock: when the pool calls drain(), it does
                // rx.close() + try_recv() to drop buffered oneshot senders.
                // But there's a TOCTOU race: this worker may have acquired
                // an mpsc buffer slot *before* close(), and completed its
                // send() *after* try_recv() finished. Now `tx` sits in the
                // buffer with nobody to consume it. The mpsc channel's
                // shared state stays alive (workers hold Sender clones),
                // so `tx` is never dropped, and `rx.await` hangs forever.
                // That prevents this worker from exiting, which keeps its
                // Sender clone alive, which keeps `tx` alive — circular.
                //
                // sender.closed() resolves when the pool's mpsc Receiver
                // is closed or dropped, letting us break out of the wait.
                //
                // biased: when both rx and closed() are ready (job arrived
                // AND shutdown in progress), always process the job first.
                // Without biased, select! picks randomly, silently dropping
                // in-flight work.
                let result = tokio::select! {
                    biased;
                    result = rx => result,
                    _ = self.sender.closed() => {
                        log::debug!("worker exiting: channel closed while awaiting job");
                        break;
                    }
                };

                match result {
                    Ok((input, result_tx)) => {
                        let output = self.work.run(input).await;
                        if let Some(tx) = result_tx {
                            let _ = tx.send(output);
                        }
                    }
                    Err(_) => {
                        log::debug!("worker handle dropped without sending a job");
                    }
                };
            }
            // _guard dropped here (normal exit) or during panic unwind
        });
    }
}
