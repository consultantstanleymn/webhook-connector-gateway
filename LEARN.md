# Learn Rust through this project

This file is for a reader who knows another language (Python, TypeScript, Java) and is new to Rust. Each part points to real code in this repo.

## 1. Ownership and borrowing

Rust has no garbage collector. Each value has one owner. When the owner goes out of scope, the value is freed.

- **Move:** `let b = a;` moves the value. You cannot use `a` after that.
- **Borrow:** `&value` lends a read-only view. `&mut value` lends one writable view. The compiler allows many readers or one writer, never both at once.

Where you see this here:

- `signature::verify(secrets: &[&[u8]], header: &str, body: &[u8], ...)`. The function only reads, so it borrows. No copy of the body is made.
- `Engine` is `#[derive(Clone)]`. Each worker task needs its own handle, and the task must own it because the task may outlive the function that spawned it. The fields are `Arc<...>` (shared, counted pointers), so a clone only adds to a counter.
- `Mutex` in `ratelimit.rs` and `breaker.rs`: shared state that changes. The lock is held for a few lines and never across an `.await`.

A common first error is "borrowed value does not live long enough". It means a reference outlives its owner. The fix is usually to own the data (`String` instead of `&str`) or to clone it.

## 2. Async/await and tokio

An `async fn` returns a future, a value that describes work to do later. Nothing runs until something polls it. `.await` says "wait here, let other tasks run".

`tokio` is the runtime. It runs many tasks on a few OS threads.

- `#[tokio::main]` starts the runtime for `main`.
- `tokio::spawn(async move { ... })` starts a task (see `worker.rs`, `spawn_pool`).
- `tokio::select!` waits for the first of several futures. A worker waits for a wake-up, a timer, or a shutdown signal (`worker.rs`).
- `watch::channel(bool)` carries the shutdown flag to all workers.
- `Notify` wakes an idle worker when a new event arrives.

Rule: never block a task with slow synchronous code (a long loop, `std::thread::sleep`). It stops other tasks on that thread. Use `tokio::time::sleep` and async libraries.

## 3. Errors with Result

Rust has no exceptions. A function that can fail returns `Result<T, E>`. The caller must handle both cases.

- `?` returns early on error and passes it up. You see it on almost every database call.
- `thiserror` (see `error.rs`, `signature.rs`) makes a clean error enum for a library or an API. Each variant has a message.
- `anyhow` (see `main.rs`, `worker.rs`) is for application code where you only need "something failed, with context".
- `impl From<sqlx::Error> for ApiError` lets `?` convert a database error into an API error.
- `impl IntoResponse for ApiError` turns an error into an HTTP response. Internal errors are logged, and the caller only sees "internal error".

## 4. Traits

A trait is a set of methods a type can implement, like an interface.

- `Mac` (from `hmac`) gives `update` and `verify_slice`.
- `IntoResponse` (axum) lets a type be returned from a handler.
- `FromRow` (sqlx) maps a database row to a struct. `#[derive(FromRow)]` writes the code for you.
- `Serialize` and `Deserialize` (serde) read and write JSON and TOML. `Rule` in `transform.rs` uses `#[serde(tag = "op")]` so `{ op = "rename", ... }` in the config becomes the right enum variant.

## 5. How an axum handler works

```rust
pub async fn ingest(
    State(st): State<AppState>,   // shared state, cloned per request
    headers: HeaderMap,           // request headers
    body: Bytes,                  // raw body (must be last)
) -> Result<(StatusCode, Json<Value>), ApiError>
```

Each argument is an "extractor". axum reads it from the request before it calls your function. The return type is anything that implements `IntoResponse`. A `Result` works when both sides do. The router in `app.rs` connects paths to handlers. A layer (`DefaultBodyLimit`, the request middleware) wraps handlers. The body limit gives 413 before the handler runs.

The handler takes the raw `Bytes` and not `Json<T>` on purpose. The signature covers the exact bytes. If the body were parsed and printed again, the bytes could change and the check would fail.

## 6. HMAC and replay protection

HMAC-SHA256 mixes a secret key and a message into a 32-byte tag. Only someone with the key can make the right tag. The customer signs `"<timestamp>.<body>"` and sends the tag. The gateway makes the same tag and compares.

Two attacks matter:

1. **Timing attack.** A normal `==` stops at the first different byte, so the time shows how many bytes were right. `verify_slice` takes the same time for every input.
2. **Replay.** An attacker records a valid request and sends it again. The timestamp is part of the signed text. The gateway refuses a timestamp more than 5 minutes from its own clock. Inside that window the idempotency key stops a second event.

Try it: `docs/ONBOARDING.md` shows the same signature with `openssl`.

## 7. Retries with jitter

If a downstream is down, a retry every second from 1,000 events would hit it all at once when it comes back. The fix:

1. Wait longer each time (exponential): 1 s, 2 s, 4 s, ... up to a cap.
2. Add randomness (jitter) so the retries spread out.

`backoff.rs` has the pure function. `proptest` checks the rules for thousands of random inputs: the delay is never above the ceiling, never below half of it, the ceiling never shrinks, and nothing overflows for huge attempt numbers.

## 8. What to say in an interview

- "I built a webhook gateway in Rust. It verifies HMAC signatures with a timestamp window, stores each event in SQL before it answers 202, and delivers from a worker pool with retries, jitter, a dead-letter table, and a circuit breaker."
- "The queue is a table. A worker claims a row with one `UPDATE ... RETURNING`. A lease lets another worker take over after a crash. That gives at-least-once delivery, and I send the event id as an idempotency key so the downstream can dedupe."
- "I chose Rust here because the service is I/O heavy, runs for months, and must not crash or leak. The type system makes error handling explicit. I used it for one service, not everywhere. My other projects use Python and TypeScript."
- "I measured it. On my 2015 laptop it accepts about 4,100 to 4,400 events per second with a p99 near 25 ms, with the load tool on the same machine. My first run was 4 times slower. The load test showed that my claim query scanned and sorted the whole backlog. I fixed it with partial indexes. Those numbers are for that laptop only." (Numbers are in docs/PERF.md.)
- "For a customer I wrote a 15-minute onboarding guide and a runbook. A Forward Deployed Engineer needs those as much as the code."
- Be ready to say what is not done (list below). Honest limits are a strength.

## 9. Honest list of simplifications

- SQLite only. Postgres is a design goal, not an implemented backend.
- Signing secrets are stored in clear in the database. A production system should encrypt them with a key from a KMS or secret manager.
- The rate limiter and the circuit breaker are in memory. With many gateway instances, each has its own state. The limit is per instance.
- The queue uses polling plus an in-process wake-up. With several instances, a new event wakes workers only on the instance that took it. Other instances find it within the poll time.
- Delivery is at-least-once. Exactly-once is not possible here. The downstream must dedupe.
- Events without an `Idempotency-Key` cannot be checked for replay inside the 5-minute window.
- No ordering guarantee. Retries and parallel workers can reorder events.
- Admin API uses one shared bearer token. There are no roles or audit log.
- No TLS in the gateway. Put it behind a load balancer or proxy that ends TLS.
- Tenant rules are read from the config at start. Changing them needs a restart.
- The transform language is small on purpose: rename, remove, enrich, keep, drop.
- Docker files and the CI file were written but not run on the build machine, which has no Docker. See README.
- Load numbers come from one old laptop with the test client on the same machine.
