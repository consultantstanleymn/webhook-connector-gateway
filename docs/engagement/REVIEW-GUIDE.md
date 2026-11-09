# Review guide (60 minutes)

Fictional customer: FieldServiceCo. Question: "Will each event arrive once, and how will we know when it does not?"

## 5-minute pitch

The gateway takes signed job events, stores each one in a durable queue, and delivers it to the customer ERP with retries. This work adds a Postgres store next to SQLite behind one port, versioned migrations, global backpressure, a per-tenant retry budget, a kill -9 failure lab, a delivery SLO with alerts, load tests, trace export, and sealed signing secrets. Delivery is at least once. The ERP removes repeats with the idempotency key. The gateway does not claim exactly-once delivery.

## Demo steps

| Step | Command |
|---|---|
| Full suite on both backends | `scripts/test-both-backends.sh` |
| Kill and redrive lab | `scripts/failure-lab-redrive.sh sqlite` (then `postgres`) |
| SLO rules | `podman run --rm -v "$PWD/deploy/rules:/r:ro,z" --entrypoint promtool docker.io/prom/prometheus:v2.55.1 test rules /r/slo.rules.test.yml` |
| Trace | `scripts/trace-run.sh /tmp/trace.json` |
| Reseal | `gateway reseal-secrets --to <kid>` |

## Likely questions

| # | Question | Short answer | Where |
|---|---|---|---|
| 1 | Is delivery exactly once? | No. It is at least once. The ERP dedupes by key. | `FAILURE-LAB.md` |
| 2 | How does the Postgres claim avoid double work? | `FOR UPDATE SKIP LOCKED`, one row for each claim. | `src/store/postgres.rs`, `tests/pg_store.rs` |
| 3 | Why an enum and not a trait object? | Async trait methods are not object safe. Two known backends. | `DECISIONS.md` entry 18 |
| 4 | How do old databases upgrade? | Migration 1 is recorded with its real checksum. | `tests/migrations.rs` |
| 5 | What happens when the queue is full? | 503 with `Retry-After`. A counter shows it. | `tests/gateway.rs` |
| 6 | Why cache the pending count for 250 ms? | Avoid a count query for each request. Accepted events bump the cache. | `src/ingest.rs` |
| 7 | What does the retry budget protect? | A hurt downstream from retry storms. | `src/retrybudget.rs` |
| 8 | Is a refused retry lost? | No. It waits for the longest backoff. | `DECISIONS.md` entry 19 |
| 9 | What did the kill lab show? | 500 of 500 events applied once at the mock, on both backends. | `evidence/failure-lab-redrive-*.txt` |
| 10 | Why was the lab kill early? | The load tool sends fast. The run is short and debug built. | `FAILURE-LAB.md` |
| 11 | Why guard the SLO alert by downstream health? | A downstream outage is not a gateway fault. | `SLO.md` |
| 12 | How do you know the alert works? | promtool: fire, quiet and downstream-down cases. | `evidence/promtool-delivery-slo.txt` |
| 13 | What are the k6 numbers? | Measured on one laptop with the load tool on the same machine. See PERF.md. | `docs/PERF.md` |
| 14 | How do spans join one trace? | The stored trace id is the parent trace id for each span. | `src/observability.rs` |
| 15 | Where are signing secrets kept? | Sealed in the database. The key is outside it. | `src/secrets.rs` |
| 16 | What if the key is lost? | Sealed values cannot open. Keep the key in a backup. | `docs/RUNBOOK.md` |
| 17 | How do you rotate the key? | New active key, restart, `reseal-secrets`, then remove the old key. | `docs/RUNBOOK.md` |
| 18 | What is not covered? | Multi-instance limits, a cloud key service, real ERPs. | `RISK-DECISION.md` |
| 19 | Did CI run for this work? | No. The new jobs are authored, not run. | README |
| 20 | Which risk did you accept? | Per-process limits with more than one instance. | `RISK-DECISION.md` |

## Remaining risks

Per-process breaker and limits. Key and data on one host. Short local runs on one laptop. Alerts proven on synthetic series only.
