# Demo script (5 minutes)

Everything below is real output from `scripts/demo.sh`, run on the build machine with the release binaries. IDs, ports, and timestamps change on each run. The script starts a mock downstream and the gateway on random free ports and stops them at the end.

```bash
cargo build --release
scripts/demo.sh
```

## What to say at each step

| Step | Say | Time |
|---|---|---|
| Start | "One gateway process. HTTP server and worker pool. Logs are JSON." | 0:30 |
| 1 | "A signed event. 202 means it is already saved in SQL." | 0:30 |
| 2 | "A repeat with the same Idempotency-Key does not create a second event." | 0:30 |
| 3, 4 | "A wrong secret and a replayed old request are both refused. Neither reaches the queue." | 0:45 |
| 5 | "Size limit is checked before anything else." | 0:15 |
| 6 | "The downstream failed twice with 503. The gateway retried with growing, random waits, and the third try worked. The trace id is the same in the log and in the downstream call." | 1:00 |
| 7 | "A tenant whose downstream is down. After 4 tries the event goes to dead letters. I start the downstream and replay with one command." | 1:00 |
| 8 | "Secret rotation. Old and new secrets both work in the grace window." | 0:30 |
| 9, 10 | "Admin API needs a token. Metrics show the queue and the outcomes." | 0:30 |
| 11 | "SIGTERM: stop the HTTP server, let workers finish, exit." | 0:15 |

## Output

```text

### Start a mock downstream that answers 503 to its first 2 calls and checks our signature

### Start the gateway
+ curl http://127.0.0.1:59181/healthz  ->  ok
+ curl http://127.0.0.1:59181/readyz   ->  ready

### 1. Send a signed webhook (expect 202)
+ X-Signature: t=1794100908,v1=0e9a211696e8ff6aef3c5b9873fae8a7fe1a0275f0b7197e02248a90fff2c271
HTTP/1.1 202 Accepted
x-request-id: 94ca070e-dcaf-425c-83a3-518769ab4cb4
{"id":"ddb58c0c-7a5b-48e9-b6b6-be5642898e23","status":"queued"}


### 2. Same Idempotency-Key again (expect 200, status duplicate, no second delivery)
{"id":"ddb58c0c-7a5b-48e9-b6b6-be5642898e23","status":"duplicate"}
HTTP 200

### 3. Bad signature (expect 401)
{"error":{"code":"bad_signature","message":"signature does not match the body and secret"}}
HTTP 401

### 4. Old timestamp, correctly signed 10 minutes ago (expect 401, replay protection)
{"error":{"code":"stale_timestamp","message":"signature timestamp is outside the allowed window"}}
HTTP 401

### 5. Body above the size limit (expect 413)
HTTP 413

### 6. The first delivery failed twice (503), then worked. Event list from the CLI:
+ gateway events
ID                                    TENANT     STATUS     TRY  LAST ERROR
ddb58c0c-7a5b-48e9-b6b6-be5642898e23  acme       delivered    3  

+ what the downstream received (mock log)
mock downstream listening on 127.0.0.1:40453
received event_id=ddb58c0c-7a5b-48e9-b6b6-be5642898e23 traceparent=00-e6e5d874f4329efcf2922d7cce35658c-95ff10977c11af8f-01 body={"customer":{},"email":"ada@example.com","order_id":1001,"source":"acme-gateway","type":"order.created"}

+ gateway log lines for the retries (JSON logs, shortened)
{'level': 'INFO', 'msg': 'event accepted'}
{'level': 'WARN', 'msg': 'delivery failed, will retry', 'attempt': 1, 'retry_in_ms': 210, 'trace_id': 'e6e5d874f4329efcf2922d7cce35658c', 'error': 'downstream status 503'}
{'level': 'WARN', 'msg': 'delivery failed, will retry', 'attempt': 2, 'retry_in_ms': 513, 'trace_id': 'e6e5d874f4329efcf2922d7cce35658c', 'error': 'downstream status 503'}
{'level': 'INFO', 'msg': 'event delivered', 'attempt': 3, 'trace_id': 'e6e5d874f4329efcf2922d7cce35658c'}

### 7. Dead letter and replay: tenant flaky has a downstream that is not running
{"id":"e6f08b95-0bb3-43e1-bdf3-755ebf90a776","status":"queued"}
HTTP 202
+ gateway dead-letters
e6f08b95-0bb3-43e1-bdf3-755ebf90a776  tenant=flaky attempts=4 replayed=false error=max attempts reached: connection failed

+ start the downstream on that port, then replay e6f08b95-0bb3-43e1-bdf3-755ebf90a776
{"id":"e6f08b95-0bb3-43e1-bdf3-755ebf90a776","status":"pending"}
+ gateway events --tenant flaky
ID                                    TENANT     STATUS     TRY  LAST ERROR
e6f08b95-0bb3-43e1-bdf3-755ebf90a776  flaky      delivered    1  
+ second mock log
mock downstream listening on 127.0.0.1:36211
received event_id=e6f08b95-0bb3-43e1-bdf3-755ebf90a776 traceparent=00-1093a12c2c8580da612d31fa6b66a637-7b226736134e6ee7-01 body={"note":"for the flaky tenant"}

### 8. Secret rotation with a grace window
+ gateway rotate-secret acme --grace-secs 600  (secret value hidden here)
new secret length: 70
old secret, inside grace window:
  HTTP 202
(note: the mock downstream still holds the old secret, so these two events fail its signature check, see the metrics)
new secret:
  HTTP 202

### 9. Admin API needs the token
no token:   HTTP 401
with token: HTTP 200

### 10. Metrics
gateway_deliveries_total{outcome="dead"} 1
gateway_deliveries_total{outcome="delivered"} 2
gateway_deliveries_total{outcome="permanent_failure"} 2
gateway_deliveries_total{outcome="retry"} 5
gateway_events_received_total{tenant="acme"} 3
gateway_events_received_total{tenant="flaky"} 1
gateway_queue_depth{status="dead"} 2
gateway_queue_depth{status="delivered"} 2
gateway_queue_depth{status="filtered"} 0
gateway_queue_depth{status="pending"} 0
gateway_queue_depth{status="processing"} 0
gateway_requests_rejected_total{reason="bad_signature"} 1
gateway_requests_rejected_total{reason="stale_timestamp"} 1

### 11. Graceful shutdown (SIGTERM)
INFO shutdown signal received, draining {}
INFO worker stopped {'worker': 0}
INFO worker stopped {'worker': 1}
INFO gateway stopped {}

done
```

## Notes on the output (so nothing surprises you)

- In step 8 the two requests return 202, but the metrics at step 10 show two events that died with `permanent_failure`. The mock downstream still checks signatures with the old secret. The gateway signs downstream calls with the tenant's current secret, so after the rotation the mock answers 401. This is real behaviour. Rotating a secret affects the downstream check too. The runbook says so.
- In step 7 the dead letter row stays in the `dead_letters` table with a `replayed_at` time after the replay. The event row itself goes back to `pending` and then `delivered`, as the `events --tenant flaky` listing shows. The metric `gateway_queue_depth{status="dead"}` counts event rows, so it shows the two events from step 8, not the replayed one.
- `retry_in_ms` values differ on each run because of jitter.
