# Design decisions

Each entry gives the choice, the reason, and the cost.

## 1. Save to SQL, then answer 202
The gateway writes the event to the database before it sends 202. If the process dies after the 202, the event is still in the table. The cost is one disk write on the request path. See PERF.md for the effect.

## 2. SQLite first, with all SQL in one module
`store.rs` holds every query. Tests and local runs need no server. The SQL is plain: `ON CONFLICT`, `RETURNING`, and no SQLite-only functions except `MAX(a, b)` in one query. A Postgres version needs a second `Store` and a `FOR UPDATE SKIP LOCKED` claim query. That port is not done. This repo does not claim Postgres support, only that the design makes it small.

## 3. SQLite settings: WAL and `synchronous=NORMAL`
This is fast and survives a process crash. A power loss can lose the last few commits. If a customer cannot accept that, use `FULL` or move to Postgres.

## 4. The queue is a table, not a broker
A table with a `status` and `next_attempt_at` column gives durable state, easy inspection with SQL, and no extra service. A worker takes work with one `UPDATE ... RETURNING` statement, so two workers never take the same row. The cost: polling, and limited scale. A broker is the right tool at high volume.

## 5. Lease for crash recovery
A claimed event gets `locked_until`. If a worker dies, the lease runs out and another worker takes the event. This gives at-least-once delivery. The downstream gets the event id in `Idempotency-Key` so it can drop repeats.

## 6. Signature format: `t=<unix>,v1=<hex>`
The signed text is `<t>.<body>`. This is close to the format several large providers use. The timestamp is inside the signature, so an attacker cannot change it. The window is 300 seconds by default. A header can carry more than one `v1`, which supports rotation on the sender side.

## 7. Constant-time compare
`Mac::verify_slice` from the `hmac` crate does the compare. The admin token compare uses a small constant-time function. Both avoid early exit on the first wrong byte.

## 8. Replay protection is the timestamp window plus idempotency keys
A captured request is valid for 5 minutes. Inside that window, the same `Idempotency-Key` returns the first event and does not create a second one. Without a key, the gateway cannot detect a replay inside the window. The docs tell customers to always send a key.

## 9. Same key, different body gives 422
This protects against a client bug that reuses keys. The gateway stores a SHA-256 of the body for this check.

## 10. Equal jitter backoff
Delay = half the ceiling plus a random part of the other half. The ceiling doubles each attempt up to a cap. "Full jitter" (0 to ceiling) gives smoother load but can retry almost at once. Equal jitter keeps a floor. The random number is a function parameter, so property tests can check the bounds.

## 11. 4xx is permanent, except 408 and 429
A 400 will not get better by waiting. Those events go to dead letters at once. 5xx, 408, 429, timeouts, and connection errors retry.

## 12. Circuit breaker per tenant, and it does not use up attempts
When the circuit is open, the worker gives the event back with the attempt count unchanged. A long downstream outage then does not push all events to dead letters. The breaker state is in memory, so each gateway instance has its own.

## 13. Tenant credentials in the database, rules in the config
Secrets must change at run time (rotation). Rules change with a deploy and are easy to review in git. The config can hold `api_key` and `secret` for demos. In real use, leave them out and let the gateway make them. API keys are stored as SHA-256 hashes. Signing secrets are stored in clear, because HMAC needs the key itself. See LEARN.md for this limit.

## 14. Rotation with a grace window
`rotate-secret` makes a new secret and keeps the old one valid for a set time (default 24 hours). The customer can switch without downtime.

## 15. Metrics labels
Labels are tenant, route, status, outcome. The route label is the route pattern (for example `/admin/events/{id}`), not the real path, so the number of series stays small.

## 16. Few dependencies, no `unsafe`
No `unsafe` in this repo. The hand-made mock downstream in tests replaces a mock library. `rustls` is used for HTTPS so the build does not need OpenSSL headers.

## 17. Versioned migrations and adoption of old database files
The schema now lives in `migrations/<backend>/NNNN_name.sql` and `sqlx::migrate!` applies it. An old database file has the tables but no `_sqlx_migrations` table. On start, the code records migration 1 as applied with the real checksum, so the DDL does not run again over existing data. Migrations are never edited after release. A change is a new numbered file.

## 18. Store port as an enum, not a trait object
The store is an enum with one variant for each backend (SQLite now, Postgres next). Rust async functions in traits are not object safe, so a `dyn Store` would need boxing or a helper crate. The backends are known and few. The compiler checks that every method handles every backend. A third backend means one new variant and one new match arm in each method.

## 19. Retry budget per tenant
When a downstream fails, every event retries, and the retries add load to a system that is already hurt. Each tenant has a budget: retries may not pass `max(min, ratio * first attempts)` in a sliding window (defaults: ratio 0.2, min 10, window 60 s). A retry that the budget refuses waits for the longest backoff, and `gateway_retry_budget_exhausted_total{tenant}` counts it. The event is not dropped. The tenant label is bounded because the tenant set comes from the config. The budget lives in one process, so with many instances the limit is approximate.

## 20. Span export is off unless an endpoint is set
When `OTEL_EXPORTER_OTLP_ENDPOINT` is set, the gateway exports spans over OTLP gRPC. Without it, no exporter starts and the logs work as before. Spans `ingest`, `claim` and `deliver` join one trace per event. Only the trace id travels with a stored event, so each span uses that trace id with a made-up remote parent span id. The outbound `traceparent` header still carries the event trace id with a new span id. Crates (all one 0.33 line, tracing-opentelemetry 0.34): opentelemetry 0.33.1, opentelemetry_sdk 0.33.1, opentelemetry-otlp 0.33.1, tracing-opentelemetry 0.34.0.

## 21. Signing secrets are sealed at rest
Tenant signing secrets are stored as `v1:<kid>:<nonce>:<ciphertext>` (AES-256-GCM, key id as associated data). The keys come from a provider: environment variables or files. The gateway does not start without the active key. A plain value from before sealing is still read (legacy) until `reseal-secrets` replaces it. The API key is stored as a hash and needs no key. A cloud key service adapter is not built. The key and the data live on the same host, so this protects database dumps and backups, not a host that is fully taken over.
