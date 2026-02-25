use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use tokio::sync::Mutex;
use workers::{Work, WorkerPool};

// ---------------------------------------------------------------------------
// Work implementations
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct Noop;

impl Work for Noop {
    type Input = ();
    type Output = ();
    fn run(&mut self, _: Self::Input) -> impl Future<Output = ()> + Send {
        async {}
    }
}

#[derive(Clone)]
struct Arithmetic;

impl Work for Arithmetic {
    type Input = (u64, u64);
    type Output = u64;
    fn run(&mut self, input: Self::Input) -> impl Future<Output = u64> + Send {
        async move { input.0.wrapping_mul(input.1).wrapping_add(42) }
    }
}

#[derive(Clone)]
struct SimulatedWork;

impl Work for SimulatedWork {
    type Input = Duration;
    type Output = ();
    fn run(&mut self, dur: Self::Input) -> impl Future<Output = ()> + Send {
        async move { tokio::time::sleep(dur).await }
    }
}

#[derive(Clone)]
struct ContendedAdder;

impl Work for ContendedAdder {
    type Input = Arc<Mutex<u64>>;
    type Output = ();
    fn run(&mut self, counter: Self::Input) -> impl Future<Output = ()> + Send {
        async move {
            let mut val = counter.lock().await;
            *val += 1;
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
}

const POOL_SIZES: &[usize] = &[1, 4, 16, 64, 256];
const JOB_COUNT: usize = 10_000;

// ---------------------------------------------------------------------------
// Benchmarks
// ---------------------------------------------------------------------------

/// Pure pool overhead: get() + send() with a noop worker.
fn bench_throughput_noop(c: &mut Criterion) {
    let mut group = c.benchmark_group("throughput/noop_send");
    group.throughput(criterion::Throughput::Elements(JOB_COUNT as u64));

    for &size in POOL_SIZES {
        let rt = rt();
        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, &size| {
            b.to_async(&rt).iter(|| async move {
                let mut pool = WorkerPool::new(Noop, size);
                for _ in 0..JOB_COUNT {
                    let w = pool.get().await.unwrap();
                    w.send(()).unwrap();
                }
                pool.close().await;
            });
        });
    }
    group.finish();
}

/// Noop get() + run() — measures result round-trip overhead.
fn bench_throughput_noop_run(c: &mut Criterion) {
    let mut group = c.benchmark_group("throughput/noop_run");
    group.throughput(criterion::Throughput::Elements(JOB_COUNT as u64));

    for &size in POOL_SIZES {
        let rt = rt();
        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, &size| {
            b.to_async(&rt).iter(|| async move {
                let mut pool = WorkerPool::new(Noop, size);
                for _ in 0..JOB_COUNT {
                    let w = pool.get().await.unwrap();
                    w.run(()).await.unwrap();
                }
                pool.close().await;
            });
        });
    }
    group.finish();
}

/// Fire-and-forget with arithmetic work.
fn bench_throughput_arithmetic(c: &mut Criterion) {
    let mut group = c.benchmark_group("throughput/arithmetic_send");
    group.throughput(criterion::Throughput::Elements(JOB_COUNT as u64));

    for &size in POOL_SIZES {
        let rt = rt();
        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, &size| {
            b.to_async(&rt).iter(|| async move {
                let mut pool = WorkerPool::new(Arithmetic, size);
                for i in 0..JOB_COUNT {
                    let w = pool.get().await.unwrap();
                    w.send((i as u64, 7)).unwrap();
                }
                pool.close().await;
            });
        });
    }
    group.finish();
}

/// Handle churn: get + drop without work. Exercises worker recovery path.
fn bench_handle_churn(c: &mut Criterion) {
    let mut group = c.benchmark_group("overhead/handle_churn");
    let churn_count = 5_000;
    group.throughput(criterion::Throughput::Elements(churn_count as u64));

    for &size in &[1, 4, 16] {
        let rt = rt();
        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, &size| {
            b.to_async(&rt).iter(|| async move {
                let mut pool = WorkerPool::new(Noop, size);
                for _ in 0..churn_count {
                    let w = pool.get().await.unwrap();
                    drop(w);
                }
                pool.close().await;
            });
        });
    }
    group.finish();
}

/// Shared-state contention: all workers increment a single counter.
fn bench_contended_state(c: &mut Criterion) {
    let mut group = c.benchmark_group("contention/shared_counter");
    group.throughput(criterion::Throughput::Elements(JOB_COUNT as u64));

    for &size in POOL_SIZES {
        let rt = rt();
        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, &size| {
            b.to_async(&rt).iter(|| async move {
                let counter = Arc::new(Mutex::new(0u64));
                let mut pool = WorkerPool::new(ContendedAdder, size);
                for _ in 0..JOB_COUNT {
                    let w = pool.get().await.unwrap();
                    w.send(Arc::clone(&counter)).unwrap();
                }
                pool.close().await;
            });
        });
    }
    group.finish();
}

/// Concurrent callers: N tasks race for workers on a shared pool.
fn bench_concurrent_callers(c: &mut Criterion) {
    let mut group = c.benchmark_group("contention/concurrent_get");
    let caller_counts: &[usize] = &[4, 16, 64];

    for &callers in caller_counts {
        let rt = rt();
        let jobs_per_caller = JOB_COUNT / callers;

        group.bench_with_input(
            BenchmarkId::from_parameter(callers),
            &callers,
            |b, &_callers| {
                b.to_async(&rt).iter(|| {
                    let jobs_per_caller = jobs_per_caller;
                    let callers = callers;
                    async move {
                        let pool = Arc::new(Mutex::new(WorkerPool::new(Noop, 16)));

                        let mut handles = Vec::with_capacity(callers);
                        for _ in 0..callers {
                            let pool = Arc::clone(&pool);
                            handles.push(tokio::spawn(async move {
                                for _ in 0..jobs_per_caller {
                                    let mut p = pool.lock().await;
                                    let w = p.get().await.unwrap();
                                    drop(p);
                                    w.send(()).unwrap();
                                }
                            }));
                        }
                        for h in handles {
                            h.await.unwrap();
                        }

                        pool.lock().await.drain();
                        pool.lock().await.wait().await;
                    }
                });
            },
        );
    }
    group.finish();
}

/// Simulated 50µs workload — shows pool size impact with real async work.
fn bench_simulated_workload(c: &mut Criterion) {
    let mut group = c.benchmark_group("realistic/simulated_50us");
    let job_count = 500;
    group.throughput(criterion::Throughput::Elements(job_count as u64));
    group.sample_size(10);
    group.warm_up_time(Duration::from_millis(500));
    group.measurement_time(Duration::from_secs(5));

    for &size in &[1, 4, 16, 64] {
        let rt = rt();
        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, &size| {
            b.to_async(&rt).iter(|| async move {
                let mut pool = WorkerPool::new(SimulatedWork, size);
                for _ in 0..job_count {
                    let w = pool.get().await.unwrap();
                    w.send(Duration::from_micros(50)).unwrap();
                }
                pool.close().await;
            });
        });
    }
    group.finish();
}

/// Pool creation + immediate close at various sizes.
fn bench_shutdown(c: &mut Criterion) {
    let mut group = c.benchmark_group("lifecycle/close");

    for &size in POOL_SIZES {
        let rt = rt();
        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, &size| {
            b.to_async(&rt).iter(|| async move {
                let pool = WorkerPool::new(Noop, size);
                pool.close().await;
            });
        });
    }
    group.finish();
}

/// Burst: saturate pool with a spike of work, then close.
fn bench_burst(c: &mut Criterion) {
    let mut group = c.benchmark_group("pattern/burst");
    let burst_size = 1_000;
    group.throughput(criterion::Throughput::Elements(burst_size as u64));

    for &size in &[4, 16, 64] {
        let rt = rt();
        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, &size| {
            b.to_async(&rt).iter(|| async move {
                let mut pool = WorkerPool::new(Arithmetic, size);
                for i in 0..burst_size {
                    let w = pool.get().await.unwrap();
                    w.send((i as u64, 13)).unwrap();
                }
                pool.close().await;
            });
        });
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_throughput_noop,
    bench_throughput_noop_run,
    bench_throughput_arithmetic,
    bench_handle_churn,
    bench_contended_state,
    bench_concurrent_callers,
    bench_simulated_workload,
    bench_shutdown,
    bench_burst,
);
criterion_main!(benches);
