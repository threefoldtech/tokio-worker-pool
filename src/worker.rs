use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use tokio::sync::{mpsc, oneshot, Notify};

use super::{Job, Work};

pub struct Worker<W: Work> {
    work: W,
    sender: mpsc::Sender<Job<W>>,
    notify: Arc<Notify>,
    count: Arc<AtomicUsize>,
}

impl<W> Worker<W>
where
    W: Work + Send + Sync + 'static,
{
    pub fn new(
        work: W,
        sender: mpsc::Sender<Job<W>>,
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

    pub fn run(self) {
        tokio::spawn(async move {
            loop {
                let (tx, rx) = oneshot::channel();

                if self.sender.send(tx).await.is_err() {
                    log::debug!("worker exiting");
                    break;
                }

                match rx.await {
                    Ok(job) => {
                        let result = self.work.run(job.0).await;
                        if let Some(ch) = job.1 {
                            let _ = ch.send(result);
                        }
                    }
                    Err(err) => {
                        log::debug!("worker handler dropped without receiving a job: {}", err);
                    }
                };
            }
            self.count.fetch_sub(1, Ordering::Relaxed);
            self.notify.notify_one();
        });
    }
}
