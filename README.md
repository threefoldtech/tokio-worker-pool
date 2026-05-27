# Grid Worker Pool

`grid_worker_pool` is a Rust library providing a configurable worker pool built on the Tokio async runtime. It offers managed concurrency primitives for async applications, handling task distribution and backpressure with a fixed number of long-lived workers.

## What this is

This crate provides bounded parallelism for async Rust applications. Each worker owns its own clone of your `Work` implementation and accepts one job at a time. The pool supports two execution styles:

- **Fire-and-forget** with `WorkerHandle::send()`
- **Request-response** with `WorkerHandle::run()`

It also provides explicit shutdown control:

- `drain()` stops accepting new work
- `wait()` waits for workers to exit
- `close()` does both for graceful shutdown

This crate is useful when you want a fixed amount of async concurrency without spawning an unbounded number of tasks for every input item. Typical cases include per-worker connection state, per-worker caches or counters, backpressure via a limited worker count, and predictable shutdown of in-flight work.

## What this repository contains

- `src/` — Library source code implementing `Work`, `WorkerPool`, and `WorkerHandle`
- `examples/` — Usage examples including stress testing
- `benches/` — Criterion benchmarks
- `Cargo.toml` / `Cargo.lock` — Rust package manifest and lock file
- `Makefile` — Build, test, and release automation

## Role in the stack

`grid_worker_pool` is a foundational concurrency library. It can be used by any Rust async service that requires controlled parallel execution, bounded resource usage, or worker-local state. It is runtime-agnostic beyond its dependency on Tokio and imposes no specific framework requirements on the applications that use it.

## Relation to ThreeFold

This technology is used within the ThreeFold ecosystem and was first deployed on the ThreeFold Grid. The component itself is designed as reusable infrastructure technology and should be understood by its technical function first, independent of any specific deployment.

## Ownership

This repository is owned and maintained by TF-Tech NV, a Belgian company responsible for the development and maintenance of this technology.

## Getting Started

Add the crate to your project:

```toml
[dependencies]
grid_worker_pool = { git = "https://github.com/threefoldtech/tokio-worker-pool" }
tokio = { version = "1", features = ["rt", "sync", "macros"] }
```

## Example

```rust
use std::future::Future;
use grid_worker_pool::{Work, WorkerPool};

#[derive(Clone)]
struct Doubler;

impl Work for Doubler {
    type Input = u64;
    type Output = u64;

    fn run(&mut self, input: Self::Input) -> impl Future<Output = Self::Output> + Send {
        async move { input * 2 }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let mut pool = WorkerPool::new(Doubler, 4);

    let worker = pool.get().await.expect("worker available");
    let output = worker.run(21).await.expect("job completed");

    assert_eq!(output, 42);

    pool.close().await;
}
```

## Development

```bash
make build
make test
make check
make ci
```

Target summary:

- `make build` — builds the crate
- `make test` — runs the test suite
- `make test-release` — runs tests in release mode
- `make check` — runs the main verification set
- `make bench-build` — compiles benches without starting Criterion
- `make stress` — runs the release stress example
- `make version` — prints the current release tag
- `make ci` — runs the local equivalent of the CI pipeline
- `make release` — runs checks, commits release metadata, and creates the git tag
- `make clean` — removes build artifacts

## CI

GitHub Actions runs on every push to `main` and on every pull request. The workflow enforces formatting, linting, and verification.

## License

This project is licensed under the Apache License 2.0 — see the [LICENSE](LICENSE) file for details.
