# Acceptance criteria

Written before the code. Status values: not started, met locally, authored not run, not met.

| ID | Criterion | Measurable check | Proof command | Evidence file | Status |
|---|---|---|---|---|---|
| R-1 | A Postgres store adapter behind the same `Store` port, with `FOR UPDATE SKIP LOCKED` claim. The full integration suite runs against SQLite and Postgres. | `cargo test` passes twice (default SQLite; `GATEWAY_TEST_PG_URL` set) | `docs/engagement/evidence/dual-backend-tests.txt` | docs/engagement/evidence/dual-backend-tests.txt | met locally |
| R-2 | Versioned migrations (`sqlx::migrate!`) replace `CREATE TABLE IF NOT EXISTS`. | `_sqlx_migrations` table exists on both backends; old DB file upgrades | `tests/migrations.rs` | see proof column | met locally |
| R-3 | Global backpressure: when pending events pass a limit, ingest returns 503 with `Retry-After`. A test fills the queue and checks the status and the metric. Drain math is written in PERF.md. | test passes | `tests/gateway.rs::backpressure_returns_503_with_retry_after` | see proof column | met locally |
| R-4 | A retry budget per tenant: retries may not pass a set share of first attempts in a window. | test passes | `tests/gateway.rs::retry_budget_caps_retries` | see proof column | met locally |
| R-5 | Redrive without duplicates: a failure lab kills the gateway with `kill -9` during delivery, then redrives every dead letter. The mock downstream records `Idempotency-Key` values and applies each event once. | lab evidence shows applied count = unique events | `scripts/failure-lab-redrive.sh`, evidence | see proof column | met locally |
| R-6 | One SLO ("99 percent of accepted events are delivered within 60 s while the downstream is up"), an alert rule and a promtool test. | promtool exit 0 | `deploy/rules/*.test.yml` | see proof column | not started |
| R-7 | A k6 load test next to the Rust load tool, with results in PERF.md. | k6 summary committed | `load/ingest.js` | see proof column | not started |
| R-8 | OTel span export to Jaeger: ingest, claim, deliver. | trace JSON with three span names | `docs/engagement/evidence/jaeger-delivery-trace.json` | docs/engagement/evidence/jaeger-delivery-trace.json | not started |
| R-9 | Signing secrets are encrypted at rest with a key from a secret provider. | DB rows hold no plain secret; rotation test passes | `tests/secrets.rs` | see proof column | not started |
| R-10 | The container image builds with podman and passes a Trivy scan. CI files are linted. | build log, trivy output | `docs/engagement/evidence/image-build.txt` | docs/engagement/evidence/image-build.txt | not started |
