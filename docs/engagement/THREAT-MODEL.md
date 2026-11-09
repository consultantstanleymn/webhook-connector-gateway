# Threat model (short)

Fictional customer. This file lists six gateway-specific threats. The main controls are in `README.md` and `docs/DECISIONS.md`.

| # | Threat | Type | Control | File | Test |
|---|---|---|---|---|---|
| 1 | A caller forges a webhook or uses a wrong secret | Spoofing | HMAC signature with a timestamp window. API key checked first. | `src/signature.rs`, `src/ingest.rs` | `gateway::rejects_bad_auth_and_signatures` |
| 2 | A sender replays a captured request | Tampering | Idempotency key with body hash. A repeat gets 200 and no new event. A changed body gets an error. | `src/store/mod.rs` | `gateway::idempotency_key_dedupes_and_detects_conflict` |
| 3 | A sender floods the queue | Denial of service | Per-tenant rate limit, and global backpressure with 503 and `Retry-After`. | `src/ratelimit.rs`, `src/ingest.rs` | `gateway::backpressure_returns_503_with_retry_after` |
| 4 | A failing downstream makes the gateway hammer it | Denial of service | Circuit breaker and retry budget. | `src/breaker.rs`, `src/retrybudget.rs` | `gateway::retry_budget_caps_retries` |
| 5 | A database dump or backup reveals signing secrets | Information disclosure | AES-256-GCM sealing. The key lives outside the database. | `src/secrets.rs` | `secrets::db_rows_hold_no_plain_secret` |
| 6 | Someone calls the admin API without the token | Elevation of privilege | Bearer token with a constant-time compare. The admin API is off without a token. | `src/admin.rs` | `gateway::admin_api_needs_token_and_lists_things` |

## Known gaps

Rate limit, breaker and retry budget state is per process. The key and the data share one host. There is no TLS in the local stack. A cloud key service adapter is not built.
