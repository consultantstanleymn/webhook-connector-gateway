# Webhook and Connector Gateway

A gateway that customer systems send events through. It checks each webhook, saves it, and delivers it to a downstream REST endpoint with retries. It is written in Rust (axum, tokio, sqlx). The store is SQLite or Postgres.

It answers one question from a customer: "If I send you an event, will it arrive, and how will I know?"

| Feature | How |
|---|---|
| Signed requests | HMAC-SHA256 over `<timestamp>.<body>`, constant-time compare, 5-minute window |
| Replay protection | Timestamp window plus `Idempotency-Key` (same key and body: no second event; same key, other body: 422) |
| Tenants | One API key and one secret each, per-tenant token-bucket rate limit, secret rotation with a grace window |
| Durable queue | Event saved to SQL before the 202. Survives a restart. |
| Workers | Pool of tasks, exponential backoff with jitter, max attempts, dead-letter table, replay |
| Delivery | Per-tenant JSON rules (rename, remove, enrich, keep, drop), timeout, circuit breaker, signed call, trace id passed on |
| Operate | CLI and HTTP admin API: list, inspect, replay, rotate. `/healthz`, `/readyz`, `/metrics` |
| Observe | JSON logs with request ids, Prometheus counters and histograms, queue depth |
| Shutdown | SIGTERM stops new requests and lets workers finish the event they hold |

## Verified locally

All rows ran on one laptop (measured on one laptop). The load tool and the services shared the machine.

| Claim | Command | Evidence | Date |
|---|---|---|---|
| Full suite on SQLite and on Postgres 16 | `scripts/test-both-backends.sh` | `docs/engagement/evidence/dual-backend-tests.txt` | 2026-11-09 |
| Postgres store with a SKIP LOCKED claim | `cargo test --test pg_store` with `GATEWAY_TEST_PG_URL` | `docs/engagement/evidence/pg-store-tests.txt` | 2026-11-09 |
| Versioned migrations, old database files adopted | `cargo test --test migrations` | `tests/migrations.rs` | 2026-11-09 |
| 503 backpressure with Retry-After, retry budget | `cargo test --test gateway backpressure retry_budget` | `tests/gateway.rs` | 2026-11-09 |
| Kill -9 redrive lab: 500 of 500 events applied once at the mock | `scripts/failure-lab-redrive.sh sqlite` and `postgres` | `docs/engagement/evidence/failure-lab-redrive-*.txt` | 2026-11-09 |
| Delivery SLO alerts proven with promtool | `promtool test rules` | `docs/engagement/evidence/promtool-delivery-slo.txt` | 2026-11-09 |
| Sealed signing secrets and reseal | `cargo test --test secrets` | `tests/secrets.rs` | 2026-11-09 |
| Security scans: gitleaks, trivy (source), cargo audit | see `docs/engagement/SECURITY-GATE.md` | `docs/engagement/evidence/security-gate-local.txt` | 2026-11-09 |
| k6 and Rust load tool against ingest (measured on one laptop) | `scripts/k6-ingest.sh` | `docs/engagement/evidence/k6-ingest.txt` | 2026-11-09 |
| OTel spans `ingest`, `claim`, `deliver` in one Jaeger trace | `scripts/trace-run.sh` | `docs/engagement/evidence/jaeger-delivery-trace.json` | 2026-11-09 |
| Compose file passes `podman compose config` and `docker compose config` | see the file | `docs/engagement/evidence/compose-config.txt` | 2026-11-09 |
| Podman image build, Trivy scan, SBOM | `podman build` and `trivy image` | `docs/engagement/evidence/image-build.txt` | 2026-11-09 |

Test counts from a real run (`scripts/test-both-backends.sh`, Postgres leg): 88 on each backend: 51 unit, 2 mock downstream, 20 integration, 3 migration, 5 Postgres store, 7 secrets.

## Authored, not run

| File | What it would do | Why it did not run |
|---|---|---|
| `.github/workflows/ci.yml` (backend matrix with a Postgres service) | Run the suite on both backends on GitHub | Authored, not run: this file is written and syntax-checked with `actionlint 1.7.12`. It was not executed against GitHub Actions. Reason: needs GitHub Actions. |
| `.github/workflows/security.yml` (CodeQL for Rust and four more jobs) | Run the security gate on GitHub | Authored, not run: this file is written and syntax-checked with `actionlint 1.7.12`. It was not executed against GitHub Actions. Reason: needs GitHub Actions. |
| Cloud key service secret provider | Fetch the sealing key from a managed key service | Not built. Only the env and file providers exist. |

## Architecture

```mermaid
flowchart LR
    C[Customer system] -- "POST /v1/webhooks<br/>X-Api-Key, X-Signature,<br/>Idempotency-Key" --> I

    subgraph G[Gateway process]
        I[Ingest handler<br/>size limit, API key,<br/>rate limit, HMAC check,<br/>idempotency] -->|"INSERT, then 202"| Q[(SQL queue<br/>events + dead_letters)]
        W[Worker pool<br/>claim, transform,<br/>breaker, deliver] <-->|"claim / result"| Q
        A[Admin API + CLI<br/>list, replay, rotate] --> Q
        M["/metrics, /healthz, /readyz"]
    end

    W -- "signed POST<br/>Idempotency-Key, traceparent" --> D[Customer downstream REST API]
    P[Prometheus] -- scrape --> M
    W -. "retry later<br/>(backoff + jitter)" .-> Q
    W -. "max attempts or 4xx" .-> DL[(dead letters)]
    O[Operator] --> A
```

Request life: the handler does the cheap checks first (size, API key, rate limit, signature). It saves the event with one SQL statement and then sends 202. A worker takes the event with one `UPDATE ... RETURNING`, applies the tenant rules, and calls the downstream. A result of 2xx ends it. A 5xx, timeout, 408, or 429 schedules a retry. Other 4xx, or too many tries, move it to dead letters. If the downstream keeps failing, the circuit opens and events wait without using up attempts.

More: [docs/DECISIONS.md](docs/DECISIONS.md) explains the choices and their cost.

## Quick start

Needs Rust stable (1.85 or newer). On Fedora: `sudo dnf install rust cargo`.

```bash
cargo build --release
cp gateway.example.toml gateway.toml        # edit tenants and rules
GATEWAY_ADMIN_TOKEN=change-me target/release/gateway serve
```

On the first start the gateway makes an API key and a secret for each tenant that has none in the config. It prints them once to the terminal. Copy them then. Only a hash of the API key is stored.

Run the scripted demo (starts everything on random ports): `scripts/demo.sh`
Send your first signed webhook: [docs/ONBOARDING.md](docs/ONBOARDING.md)

## CLI

```text
gateway serve                              run the server and the workers
gateway events [--status S] [--tenant T]   list events
gateway show <id>                          one event with its payload
gateway dead-letters                       list dead letters
gateway replay <id> | --all                put dead events back in the queue
gateway rotate-secret <tenant> [--grace-secs N]
gateway sign --secret S --body-file F      print a signature header
```

`--config` (or `GATEWAY_CONFIG`) selects the config file. Env overrides: `GATEWAY_DATABASE_URL`, `GATEWAY_ADMIN_TOKEN`, `GATEWAY_BIND`.

## HTTP API

| Method and path | Auth | Purpose |
|---|---|---|
| `POST /v1/webhooks` | API key and signature | Send an event. 202 new, 200 duplicate |
| `GET /healthz`, `/readyz`, `/metrics` | none | Liveness, readiness (database), Prometheus |
| `GET /admin/events?status=&tenant=&limit=` | admin bearer token | List |
| `GET /admin/events/{id}` | admin | One event with payload |
| `GET /admin/dead-letters` | admin | Dead letters |
| `POST /admin/events/{id}/replay` | admin | Replay a dead event |
| `POST /admin/tenants/{id}/rotate-secret?grace_secs=` | admin | Rotate, returns the new secret once |

Keep `/admin` and `/metrics` on a private network. The gateway does not end TLS.

## Quality checks (run on the build machine)

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test
```

Results at the last run: fmt clean, clippy with `-D warnings` clean, 88 tests passed on each backend (51 unit and property, 2 mock downstream, 20 integration, 3 migration, 5 Postgres store, 7 secrets), 0 failed. See `docs/engagement/evidence/dual-backend-tests.txt`. `cargo audit` ran with one dated exception (see `docs/engagement/evidence/security-gate-local.txt`). `cargo deny` is not installed, so it did not run. The image was built with podman and scanned with Trivy (`docs/engagement/evidence/image-build.txt`). The compose file passes `podman compose config` and `docker compose config` (`docs/engagement/evidence/compose-config.txt`). The stack was not started with Docker. No `unsafe` code.

Integration tests start the real router on a random port with a temp SQLite file and a hand-made downstream, and cover: signing, replay window, idempotency, rate limit, size limit, retries, dead letter and replay, permanent 4xx, filtering, circuit breaker, rotation grace, admin auth, metrics, restart with queued events, and graceful shutdown with work in flight. Property tests (proptest) cover signatures (round trip, any body change, wrong secret, window edges, parser never panics) and backoff (bounds, monotonic ceiling, no overflow).

## Performance

About 4,100 to 4,400 accepted events per second with p99 near 25 ms, and about 2,200 events/s delivery drain (measured on one laptop, a 2015 MacBook, with the load tool on the same machine). Details, method, and limits: [docs/PERF.md](docs/PERF.md).

## Docker

```bash
GATEWAY_ADMIN_TOKEN=change-me docker compose up --build
```

Starts the gateway on :8080, a mock downstream on :9000, and Prometheus on :9090. Demo credentials are in `deploy/gateway.compose.toml` (API key `demo-api-key`, secret `demo-secret`). Replace them for anything real.

## Layout

```text
src/            library: signature, backoff, ratelimit, breaker, transform, store,
                worker, delivery, ingest, admin, app, metrics, observability
src/main.rs     CLI and server
src/bin/        mock-downstream, loadtest
tests/          integration tests (in-process server and mock downstream)
docs/           ONBOARDING, RUNBOOK, DEMO, DECISIONS, PERF
LEARN.md        Rust walk-through for a newcomer, interview notes, honest limits
scripts/        demo.sh, perf.sh
deploy/         compose config, Prometheus config
```

## Known limits

SQLite and Postgres stores exist. The signing-secret key comes from an environment variable or a file. A cloud key service adapter is not built. Rate limit and breaker state are per instance. Delivery is at-least-once with no ordering. Full list in [LEARN.md](LEARN.md).

## License

MIT. Copyright (c) 2026 Stanley Sujith Nelavala.
