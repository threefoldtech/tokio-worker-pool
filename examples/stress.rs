use std::time::Instant;
use workers::{Work, WorkerPool};

#[derive(Clone)]
struct Noop;

impl Work for Noop {
    type Input = ();
    type Output = ();
    async fn run(&mut self, _: Self::Input) {}
}

/// Test 1: everything inside #[tokio::main] (like the original stress test)
async fn stress_async(label: &str) {
    let job_count = 10_000;
    let iterations = 500;

    for &size in &[1, 4, 16, 64, 128, 256] {
        let start = Instant::now();
        for _ in 0..iterations {
            let mut pool = WorkerPool::new(Noop, size);
            for _ in 0..job_count {
                let w = pool.get().await.unwrap();
                w.send(()).unwrap();
            }
            pool.close().await;
        }
        println!(
            "[{label}] pool={size:>3}  {iterations} iters  {:.2?}/iter",
            start.elapsed() / iterations
        );
    }
}

/// Test 2: repeated block_on calls (like criterion's iter_custom)
fn stress_block_on(label: &str) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();

    let job_count = 10_000;
    let iterations = 500;

    for &size in &[1, 4, 16, 64, 128, 256] {
        let start = Instant::now();
        for _ in 0..iterations {
            rt.block_on(async {
                let mut pool = WorkerPool::new(Noop, size);
                for _ in 0..job_count {
                    let w = pool.get().await.unwrap();
                    w.send(()).unwrap();
                }
                pool.close().await;
            });
        }
        println!(
            "[{label}] pool={size:>3}  {iterations} iters  {:.2?}/iter",
            start.elapsed() / iterations
        );
    }
}

/// Test 3: single block_on, many iterations inside (like criterion's to_async)
fn stress_single_block_on(label: &str) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();

    let job_count = 10_000;
    let iterations = 500;

    for &size in &[1, 4, 16, 64, 128, 256] {
        let start = Instant::now();
        rt.block_on(async {
            for _ in 0..iterations {
                let mut pool = WorkerPool::new(Noop, size);
                for _ in 0..job_count {
                    let w = pool.get().await.unwrap();
                    w.send(()).unwrap();
                }
                pool.close().await;
            }
        });
        println!(
            "[{label}] pool={size:>3}  {iterations} iters  {:.2?}/iter",
            start.elapsed() / iterations
        );
    }
}

fn main() {
    // This one uses #[tokio::main] internally
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(stress_async("tokio::main style"));

    println!();
    stress_block_on("repeated block_on");

    println!();
    stress_single_block_on("single block_on");
}
