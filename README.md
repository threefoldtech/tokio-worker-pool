# workers

`workers` is a small Tokio worker pool for bounded parallelism with a fixed
number of long-lived workers. Each worker owns its own clone of your `Work`
implementation and accepts one job at a time.

The pool supports two execution styles:

- fire-and-forget with `WorkerHandle::send()`
- request-response with `WorkerHandle::run()`

It also has explicit shutdown control:

- `drain()` stops accepting new work
- `wait()` waits for workers to exit
- `close()` does both for graceful shutdown

## Getting started

Add the crate to your project:

```toml
[dependencies]
workers = { path = "../tokio-worker-pool" }
tokio = { version = "1", features = ["rt", "sync", "macros"] }
```

If you are using it from this repository directly, the main entrypoints are:

```bash
make build
make test
make check
make version
```

## Why this crate exists

This crate is useful when you want a fixed amount of async concurrency without
spawning an unbounded number of tasks for every input item. Typical cases:

- per-worker connection state
- per-worker caches or counters
- backpressure via a limited worker count
- predictable shutdown of in-flight work

## Concepts

### What a worker pool is

A worker pool is a way to say "only let this many pieces of work run at the
same time." Instead of spawning a fresh task for every job and letting the
runtime sort it out, you create a fixed set of workers and feed jobs to them.

That gives you two practical benefits:

- concurrency stays bounded
- worker state can live longer than a single job

### Why fixed workers can be better than spawning per job

Spawning per job is often fine, but sometimes you want stronger structure:

- you want to cap resource usage
- you want each worker to reuse expensive state
- you want shutdown to mean "finish what started, then stop"

This crate is aimed at those cases.

### Worker-local state vs shared state

This is the most important mental model in the crate.

Your `Work` value is cloned once per worker. That means:

- fields inside `self` are worker-local
- values passed in `Input` can be shared across workers

Use worker-local state for things like:

- a connection owned by one worker
- a small cache private to one worker
- a counter used only for that worker's own behavior

Use shared input state for things like:

- a common accumulator
- a shared queue or map
- data that must be seen consistently by all workers

If you need shared mutable state, pass something like `Arc<Mutex<T>>` in the
input rather than assuming `&mut self` is shared across the whole pool.

### Backpressure in plain terms

Backpressure just means the system has a natural place where it says "not yet."

Here, the pressure point is `get().await`. If all workers are busy, you wait
until one becomes available. That prevents the caller from piling up unlimited
active work.

### Fire-and-forget vs request-response

Use `send()` when the caller only cares that the job was accepted by a worker.
Use `run()` when the caller needs a result back.

Conceptually:

- `send()` is "please do this"
- `run()` is "please do this and tell me what happened"

### Graceful shutdown

Graceful shutdown means the pool stops taking new work but does not abandon work
that already started.

In this crate:

- `drain()` closes the front door
- existing jobs continue to completion
- `wait()` waits for all workers to leave
- `close()` is the full graceful shutdown path

This matters in real systems because "drop everything immediately" is often the
wrong behavior for jobs that touch files, sockets, or external services.

## How it works

The pool does not push jobs directly into a shared work queue. Instead, idle
workers advertise availability back to the pool.

Flow:

1. Each worker creates a fresh `oneshot` channel.
2. The worker sends the `oneshot::Sender` into the pool's `mpsc` queue.
3. `WorkerPool::get()` receives one of those senders and returns a
   `WorkerHandle`.
4. The caller uses that handle to send either:
   - an input only, for `send()`
   - an input plus a result channel, for `run()`
5. The worker executes the job, optionally sends back the result, and then
   advertises itself again.

That means the `mpsc` queue is a queue of available workers, not a queue of
jobs.

## Example

```rust
use std::future::Future;
use workers::{Work, WorkerPool};

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

## Choosing a pool size

Pool size is a policy decision, not a cosmetic parameter.

Reasonable starting points:

- CPU-heavy work: start near the number of CPU cores
- IO-heavy work: start somewhat higher, then measure
- expensive per-worker resources: keep the pool as small as those resources
  require

Questions to ask:

- how many jobs should run concurrently at most?
- does each worker hold scarce state, like a socket or file handle?
- is the bottleneck CPU, remote latency, or some external rate limit?

Practical advice:

- start small
- measure throughput and latency
- only increase size when there is evidence that more concurrency helps

A larger pool is not automatically better. It can increase contention, memory
use, and pressure on downstream services.

## Usage patterns

### Pattern: private per-worker state

This is the best fit for the crate.

Examples:

- one DB connection per worker
- one parser cache per worker
- one client object per worker

Put that state inside the `Work` implementation so each worker owns its own
copy.

### Pattern: shared aggregation

Sometimes workers do independent work but update a common result. In that case,
pass shared state through `Input`.

Examples:

- `Arc<Mutex<Vec<_>>>`
- `Arc<Mutex<HashMap<_, _>>>`
- `Arc<AtomicUsize>`

Keep the shared critical section small. If every job spends most of its time
waiting on the same mutex, the pool size stops mattering much.

### Pattern: request-response jobs

Use `run()` when the caller needs the result of each job.

This is a good fit for:

- transformations
- RPC-like units of work
- computations where the caller needs the output immediately

### Pattern: fire-and-forget jobs

Use `send()` when the caller wants bounded concurrency but does not need a per-job
return value.

This is a good fit for:

- background writes
- event handling
- bounded side effects

## Common mistakes

### Assuming `&mut self` is shared across the whole pool

It is not. Each worker has its own clone.

If you want truly shared mutable state, pass it through `Input` with `Arc<_>`.

### Forgetting to close the pool

Dropping the pool is allowed, but it is not a blocking shutdown path. If you
need deterministic cleanup, call `close().await`.

### Using a giant pool to hide slow downstream systems

If the bottleneck is an external service, a larger pool can just create a
larger traffic spike. Size the pool according to the downstream limit, not only
local machine capacity.

### Holding one big shared lock around all the real work

That defeats the purpose of the pool. If every job serializes on one mutex, the
effective concurrency becomes one.

### Ignoring the `Option` from `get()`

`get().await` returning `None` means the pool is drained or all workers are
gone. That is a lifecycle signal, not a transient inconvenience.

## Shutdown recipes

### Recipe: normal graceful shutdown

```rust
pool.close().await;
```

Use this when you are done submitting work and want the pool to finish in-flight
jobs cleanly.

### Recipe: stop taking work first, wait later

```rust
pool.drain();

// do other shutdown coordination here

pool.wait().await;
```

Use this when shutdown has multiple phases and the pool is only one part of it.

### Recipe: producer loop that exits cleanly

```rust
while let Some(worker) = pool.get().await {
    worker.send(job_source.next().await?).unwrap();
}
```

This style makes the pool lifecycle explicit: once the pool is drained, the
producer naturally stops.

## Operational notes

### What happens if a worker panics

The worker exits, the pool's live worker count is decremented, and waiters are
notified. The pool can continue operating with the remaining workers.

If every worker dies, `get().await` eventually returns `None`.

### What happens if a handle is dropped

Dropping a `WorkerHandle` without calling `send()` or `run()` is allowed. The
worker notices the dropped `oneshot` and advertises itself again.

### What this crate does not do

This crate deliberately does not provide:

- automatic job retries
- dynamic worker resizing
- cancellation of jobs already running
- job prioritization
- a multi-producer shared job queue API

Those can be built around the pool if needed, but they are not part of its
core contract.

## API overview

### `Work`

Implement `Work` to define what a worker does:

```rust
pub trait Work: Send + Sync + Clone + 'static {
    type Input: Send + 'static;
    type Output: Send + 'static;
    fn run(&mut self, input: Self::Input) -> impl Future<Output = Self::Output> + Send;
}
```

Important detail: each worker receives its own clone of the `Work`
implementation. Mutating `&mut self` changes that worker-local clone only.
Shared state should be passed through `Input`, usually inside `Arc<_>`.

### `WorkerPool`

- `WorkerPool::new(work, size)` creates `size` workers
- `get().await` returns `Option<WorkerHandle<W>>`
- `worker_count()` reports how many workers are still alive
- `is_drained()` reports whether the pool still accepts work
- `drain()` closes the pool to new work
- `wait().await` waits for all workers to exit
- `close().await` performs graceful shutdown

### `WorkerHandle`

- `send(job)` queues work and returns immediately
- `run(job).await` queues work and waits for the output

Handles are single-use. Dropping a handle without sending work is valid; the
worker detects that and re-registers itself with the pool.

## Lifecycle and shutdown semantics

### Normal operation

- Workers live for the lifetime of the pool
- Each worker processes at most one job at a time
- After finishing a job, the worker becomes available again

### `drain()`

`drain()` means:

- no new `get()` calls can succeed after buffered worker slots are exhausted
- workers currently running a job are allowed to finish
- idle workers exit once they observe shutdown

After `drain()`, `get().await` returns `None`.

### `wait()`

`wait()` only waits for worker exit. It does not itself initiate shutdown. In
practice you usually call:

```rust
pool.drain();
pool.wait().await;
```

or just:

```rust
pool.close().await;
```

### Dropping the pool

Dropping `WorkerPool` triggers `drain()` but does not block waiting for workers.
If you need deterministic shutdown, call `close()`.

## Internals

### Core data structures

- `mpsc::Receiver<WorkerSlot<W>>` in the pool:
  receives availability signals from idle workers
- `WorkerSlot<W> = oneshot::Sender<Task<W>>`:
  a single-use channel to send one task to one worker
- `Task<W>`:
  the input plus an optional result sender
- `AtomicUsize`:
  tracks how many workers are still alive
- `Notify`:
  wakes waiters when workers exit

### Why the `mpsc` capacity equals the pool size

The queue holds idle worker slots. At most `size` workers can be idle at once,
so `mpsc::channel(size)` is the natural bound. A smaller buffer can create
artificial contention among workers trying to announce availability.

### Why workers wait on `select!`

Once a worker advertises availability, it waits for either:

- a task to arrive on its private `oneshot` receiver
- the pool receiver to close

This is implemented with:

```rust
tokio::select! {
    biased;
    result = rx => result,
    _ = self.sender.closed() => break,
}
```

This matters for shutdown. If the pool closes while a worker is waiting for a
job, `sender.closed()` lets that worker break out instead of waiting forever.
The `biased` branch order preserves in-flight work when both events become ready
at the same time.

### Deadlock avoidance during shutdown

The current shutdown logic addresses three distinct failure modes:

1. Buffered worker slots retained by the `mpsc` queue could keep `oneshot`
   senders alive and leave workers blocked forever. `drain()` fixes this by
   calling `close()` on the receiver and then `try_recv()` in a loop to drop any
   buffered worker slots.
2. A waiter could miss the final worker-exit notification if it checked the
   atomic count before registering for wakeup. `wait()` fixes this by enabling
   the `Notify` waiter before checking `worker_count`.
3. A worker could race with `drain()` and manage to enqueue its worker slot
   after the receiver was closed and after the pool finished draining buffered
   slots. Waiting on `sender.closed()` inside the worker breaks that cycle.

### Panic handling

Each worker task owns a `WorkerGuard`. If the worker panics, the guard still
drops during unwinding, which:

- decrements the worker count
- notifies any task waiting in `wait()`

That keeps pool shutdown and `worker_count()` accurate even when workers die.

## Development

The project is driven by the [Makefile](/home/xmonader/wspace/geomind/tokio-worker-pool/Makefile):

```bash
make build
make test
make test-release
make check
make stress
```

Target summary:

- `make build` builds the crate
- `make test` runs the test suite
- `make test-release` runs tests in release mode
- `make check` runs the main verification set used in this repo
- `make bench-build` compiles benches without starting Criterion
- `make stress` runs the release stress example
- `make version` prints the current release tag
- `make release` runs checks, commits release metadata, and creates the git tag
- `make clean` removes build artifacts

The repo also includes:

- `examples/stress.rs` for repeated lifecycle stress testing
- `benches/pool_bench.rs` for Criterion benchmarks

## License

See [LICENSE](LICENSE).
