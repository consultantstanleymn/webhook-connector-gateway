# Send your first signed webhook in 15 minutes

This guide is for a customer engineer. At the end, your system has sent a signed event, and it has reached your endpoint.

You need: `curl`, `openssl`, and these three values from your gateway contact. Do not share them or commit them to git.

| Value | Example | Use |
|---|---|---|
| Gateway address | `https://gateway.example.com` | Where you send events |
| API key | `gwk_...` | Says who you are. Sent in `X-Api-Key` |
| Signing secret | `whsec_...` | Proves the event is yours. Never sent. |

```bash
export GW=https://gateway.example.com
export API_KEY=gwk_...
export SECRET=whsec_...
```

## Step 1. Check the gateway is up (1 minute)

```bash
curl -s $GW/readyz
```

Expected: `ready`.

## Step 2. Make a body (1 minute)

The body must be a JSON object. The size limit is 256 KiB.

```bash
BODY='{"type":"order.created","order_id":1001,"customer":{"email":"ada@example.com"}}'
```

## Step 3. Sign it (3 minutes)

The signature is an HMAC-SHA256 of the text `<timestamp>.<body>`, with your secret as the key. The timestamp is the current Unix time in seconds.

```bash
TS=$(date +%s)
HASH=$(printf '%s.%s' "$TS" "$BODY" | openssl dgst -sha256 -hmac "$SECRET" -hex | sed 's/^.* //')
SIG="t=$TS,v1=$HASH"
echo "$SIG"
```

Rules:

- Sign the exact bytes you send. If a proxy or library changes the JSON (spaces, key order), the check fails.
- Your clock must be within 5 minutes of the gateway clock. Use NTP.
- Make a new signature for each request. An old one is refused.

## Step 4. Send it (2 minutes)

Always send an `Idempotency-Key`. If your system retries, the gateway uses the key to avoid a second event.

```bash
curl -i -X POST $GW/v1/webhooks \
  -H "X-Api-Key: $API_KEY" \
  -H "X-Signature: $SIG" \
  -H "Idempotency-Key: order-1001" \
  -H "Content-Type: application/json" \
  -d "$BODY"
```

Expected: `HTTP/1.1 202 Accepted` and a body like `{"id":"8cad6dd6-...","status":"queued"}`.

202 means the gateway has saved the event. Delivery to your endpoint happens next. You do not need to wait.

Send the same command again. Expected: `200` and `"status":"duplicate"` with the same `id`.

## Step 5. If something is wrong (3 minutes)

| Status | `error.code` | What to do |
|---|---|---|
| 401 | `missing_api_key`, `invalid_api_key` | Check the `X-Api-Key` header |
| 401 | `missing_signature`, `malformed_signature` | The header must look like `t=1700000000,v1=<64 hex characters>` |
| 401 | `bad_signature` | Wrong secret, or the signed text is not the sent body |
| 401 | `stale_timestamp` | Check your clock. Sign again |
| 400 | `bad_request` | Body is not a JSON object |
| 413 | | Body is above 256 KiB |
| 422 | `idempotency_conflict` | You used the same key with a different body |
| 429 | `rate_limited` | Wait for the number of seconds in the `Retry-After` header |
| 5xx | | Retry with the same `Idempotency-Key`, and wait a little longer each time |

Send `X-Request-Id: <your id>` to find the request in the gateway logs. The gateway sends the id back in the response.

## Step 6. Check delivery (3 minutes)

Your endpoint gets a `POST` with the transformed JSON and these headers:

| Header | Meaning |
|---|---|
| `X-Gateway-Signature` | Same format as above, signed with your secret. Verify it. |
| `X-Gateway-Event-Id` | The event id from the 202 answer |
| `Idempotency-Key` | Same as the event id. Use it to drop repeats. |
| `traceparent` | W3C trace context. The trace id is the one you sent in `traceparent`, if any. |

Your endpoint must answer `2xx` within the timeout (10 seconds by default). On `5xx`, `408`, `429`, or a timeout, the gateway tries again later with growing waits. On other `4xx`, it stops and keeps the event in the dead letter list for your gateway contact to inspect. Delivery is at-least-once, so your endpoint can see the same event more than once. Use the event id to ignore repeats.

## Verify the gateway's signature in your endpoint (Python sketch)

```python
import hmac, hashlib, time

def verify(secret: bytes, header: str, body: bytes, tolerance=300) -> bool:
    parts = dict(p.split("=", 1) for p in header.split(","))
    ts = int(parts["t"])
    if abs(time.time() - ts) > tolerance:
        return False
    expected = hmac.new(secret, f"{ts}.".encode() + body, hashlib.sha256).hexdigest()
    return hmac.compare_digest(expected, parts["v1"])
```

Use a constant-time compare (`compare_digest`), not `==`.

## Rotating your secret

Your gateway contact runs the rotation. You get a new secret. The old one keeps working for a grace time (24 hours by default). Switch your sender to the new secret in that time. If your endpoint also verifies `X-Gateway-Signature`, switch it too.

## Done

You have sent a signed event, seen the 202, checked a duplicate, and know the error codes. Next: ask for a production API key and agree the rate limit.
