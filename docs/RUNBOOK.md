# Runbook

For the person who operates the gateway. Each section: symptom, check, fix.

Set these first. `GW` is the gateway address. `TOKEN` is the admin token.

```bash
export GW=http://127.0.0.1:8080
export TOKEN=...            # the value of GATEWAY_ADMIN_TOKEN
alias adm='curl -s -H "Authorization: Bearer $TOKEN"'
```

The same actions exist as CLI commands. They read the database directly, so run them on the host that has the config file: `gateway --config gateway.toml events --status dead`.

## Quick health check

```bash
curl -s $GW/healthz            # "ok" = process is up
curl -s $GW/readyz             # "ready" = database answers
curl -s $GW/metrics | grep gateway_queue_depth
```

## 1. Stuck queue

**Symptom:** `gateway_queue_depth{status="pending"}` keeps growing, or events stay `pending` or `processing`.

**Check:**

1. Are workers running? Look for log lines with `"message":"event delivered"` or `"worker stopped"`.
2. Is the circuit open? Look for `circuit open, delivery postponed`. This means the downstream failed several times in a row. Events wait. They do not use up attempts.
3. Is the downstream slow? Compare `gateway_delivery_duration_seconds` with `delivery.timeout_ms`.
4. Is the oldest event waiting for its retry time? `gateway events --status pending` shows the try count and last error. A retry after a failure is scheduled, so it is not stuck.
5. Events in `processing` for longer than `queue.lease_ms` (default 60 s) are taken again by another worker. If the gateway was killed, wait for the lease to end.

**Fix:**

- Downstream is down: fix the downstream. The circuit closes by itself after `breaker_cooldown_ms`. No action in the gateway is needed.
- Not enough workers: raise `queue.workers`, restart. Check the downstream can take the load first.
- Database locked or slow: check disk space and the size of the `-wal` file next to the database.

## 2. Dead letters

**Symptom:** `gateway_queue_depth{status="dead"}` is above 0.

**Look:**

```bash
adm "$GW/admin/dead-letters" | head -c 2000
# or
gateway --config gateway.toml dead-letters
```

Read `last_error`:

| Error | Meaning | Action |
|---|---|---|
| `max attempts reached: downstream status 503` | Downstream was down for too long | Fix downstream, then replay |
| `downstream status 400` or `404` | The downstream refuses the event or the URL is wrong | Fix the transform rules or `downstream_url`, restart, then replay |
| `downstream status 401` | Downstream rejects the gateway signature | Check the downstream uses the current tenant secret |
| `tenant is not in the config` | A tenant was removed from the config | Add it back, restart, then replay |

**Replay:**

```bash
adm -X POST "$GW/admin/events/<event_id>/replay"
# all dead events at once (CLI):
gateway --config gateway.toml replay --all
```

A replayed event starts again with 0 attempts. The downstream gets the same event id in `Idempotency-Key`. Tell the customer's team that they may see a repeat.

Replay a few events first. If the cause is not fixed, they will die again.

## 3. Secret rotation

Use it on a schedule, or at once after a leak.

1. Rotate. The old secret stays valid for the grace time.
   ```bash
   adm -X POST "$GW/admin/tenants/acme/rotate-secret?grace_secs=86400"
   # CLI: gateway --config gateway.toml rotate-secret acme --grace-secs 86400
   ```
   The answer holds the new secret. It is shown once. Send it to the customer over a safe channel. Do not paste it in chat or in a ticket.
2. The customer changes their sender to the new secret.
3. Check that new events arrive: `adm "$GW/admin/events?tenant=acme&limit=5"`.
4. When the grace time ends, the old secret stops working by itself.

**After a leak:** rotate with `grace_secs=0`. The old secret stops at once. Events signed with it get 401 until the customer updates. Also check `gateway_requests_rejected_total{reason="bad_signature"}` for attack traffic.

Downstream calls are signed with the tenant secret too. If the customer's downstream checks that signature, it must change to the new secret as well.

## 4. Rejected traffic

`gateway_requests_rejected_total{reason=...}` shows why requests are refused.

| Reason | Usual cause |
|---|---|
| `bad_signature` | Wrong secret, or the body changed after signing (for example a proxy that reformats JSON) |
| `stale_timestamp` | Sender clock is off by more than 5 minutes, or a replay |
| `rate_limited` | Tenant sends faster than `rate_limit`. Raise it or ask them to slow down |
| `idempotency_conflict` | Same key with a different body. A bug in the sender |
| `invalid_api_key` / `missing_api_key` | Wrong or missing `X-Api-Key` |

A body larger than `server.max_body_bytes` gets 413 from the framework before the handler runs.

## 5. Planned restart

Send SIGTERM (`systemctl stop`, `docker stop`). The gateway stops accepting new requests, lets each worker finish its event (up to `queue.shutdown_grace_secs`), and exits. Events that are not finished stay in the table and are taken again after the lease ends.

## 6. Backup

The database file is the state. Copy it with SQLite's backup command, not a plain file copy while the gateway runs:

```bash
sqlite3 gateway.db ".backup 'gateway-backup.db'"
```
