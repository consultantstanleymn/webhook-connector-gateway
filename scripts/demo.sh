#!/usr/bin/env bash
# Scripted demo. Run from the repo root after `cargo build --release`.
# It starts a mock downstream and the gateway on random free ports,
# walks through the main features, and stops everything (by PID).
set -u
export GATEWAY_SECRET_PRIMARY="${GATEWAY_SECRET_PRIMARY:-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=}"  # demo key for local runs, not a secret
BIN=${BIN:-target/release}
WORK=$(mktemp -d)
free_port() { python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1]);s.close()'; }
GW_PORT=$(free_port); MOCK_PORT=$(free_port); DEAD_PORT=$(free_port)
GW=http://127.0.0.1:$GW_PORT
API_KEY=demo-api-key; SECRET=demo-secret; ADMIN=demo-admin-token

cat > "$WORK/gateway.toml" <<CFG
database_url = "sqlite://$WORK/gateway.db"
[server]
bind = "127.0.0.1:$GW_PORT"
admin_token = "$ADMIN"
[queue]
workers = 2
max_attempts = 4
backoff_base_ms = 300
backoff_max_ms = 2000
poll_ms = 100
[[tenants]]
id = "acme"
api_key = "$API_KEY"
secret = "$SECRET"
downstream_url = "http://127.0.0.1:$MOCK_PORT/events"
rules = [
  { op = "keep",   field = "type", equals = "order.created" },
  { op = "rename", from = "customer.email", to = "email" },
  { op = "enrich", field = "source", value = "acme-gateway" },
]
[[tenants]]
id = "flaky"
api_key = "flaky-api-key"
secret = "flaky-secret"
downstream_url = "http://127.0.0.1:$DEAD_PORT/events"
CFG

PIDS=()
cleanup() { for p in "${PIDS[@]}"; do kill "$p" 2>/dev/null; done; wait 2>/dev/null; }
trap cleanup EXIT

say() { printf '\n### %s\n' "$*"; }
sign() { # sign <secret> <body>  -> header value
  local t; t=$(date +%s)
  local h; h=$(printf '%s.%s' "$t" "$2" | openssl dgst -sha256 -hmac "$1" -hex | sed 's/^.* //')
  echo "t=$t,v1=$h"
}
gateway() { "$BIN/gateway" --config "$WORK/gateway.toml" "$@"; }
adm() { curl -s -H "Authorization: Bearer $ADMIN" "$@"; }

say "Start a mock downstream that answers 503 to its first 2 calls and checks our signature"
FAIL_FIRST=2 WEBHOOK_SECRET=$SECRET BIND=127.0.0.1:$MOCK_PORT "$BIN/mock-downstream" > "$WORK/mock.log" 2>&1 & PIDS+=($!)
say "Start the gateway"
RUST_LOG=info "$BIN/gateway" --config "$WORK/gateway.toml" serve > "$WORK/gateway.log" 2>"$WORK/gateway.err" & GWPID=$!; PIDS+=($GWPID)
for _ in $(seq 100); do curl -sf $GW/readyz >/dev/null && break; sleep 0.1; done
echo "+ curl $GW/healthz  ->  $(curl -s $GW/healthz)"
echo "+ curl $GW/readyz   ->  $(curl -s $GW/readyz)"

BODY='{"type":"order.created","order_id":1001,"customer":{"email":"ada@example.com"}}'
say "1. Send a signed webhook (expect 202)"
SIG=$(sign $SECRET "$BODY")
echo "+ X-Signature: $SIG"
curl -s -i -X POST $GW/v1/webhooks -H "X-Api-Key: $API_KEY" -H "X-Signature: $SIG" \
  -H "Idempotency-Key: order-1001" -H "Content-Type: application/json" -d "$BODY" | grep -iE '^(HTTP|x-request-id|\{)' | tr -d '\r'
echo

say "2. Same Idempotency-Key again (expect 200, status duplicate, no second delivery)"
curl -s -w '\nHTTP %{http_code}\n' -X POST $GW/v1/webhooks -H "X-Api-Key: $API_KEY" -H "X-Signature: $(sign $SECRET "$BODY")" \
  -H "Idempotency-Key: order-1001" -d "$BODY"

say "3. Bad signature (expect 401)"
curl -s -w '\nHTTP %{http_code}\n' -X POST $GW/v1/webhooks -H "X-Api-Key: $API_KEY" -H "X-Signature: $(sign wrong-secret "$BODY")" -d "$BODY"

say "4. Old timestamp, correctly signed 10 minutes ago (expect 401, replay protection)"
OLD=$(( $(date +%s) - 600 ))
OLDSIG="t=$OLD,v1=$(printf '%s.%s' "$OLD" "$BODY" | openssl dgst -sha256 -hmac $SECRET -hex | sed 's/^.* //')"
curl -s -w '\nHTTP %{http_code}\n' -X POST $GW/v1/webhooks -H "X-Api-Key: $API_KEY" -H "X-Signature: $OLDSIG" -d "$BODY"

say "5. Body above the size limit (expect 413)"
python3 -c 'print("{\"type\":\"order.created\",\"pad\":\"" + "x"*300000 + "\"}", end="")' > "$WORK/big.json"
BIGSIG="t=$(date +%s),v1=ignored"   # the size limit is checked before the signature
curl -s -o /dev/null -w 'HTTP %{http_code}\n' -X POST $GW/v1/webhooks -H "X-Api-Key: $API_KEY" -H "X-Signature: $BIGSIG" --data-binary @"$WORK/big.json"

say "6. The first delivery failed twice (503), then worked. Event list from the CLI:"
sleep 3
echo "+ gateway events"
gateway events
echo; echo "+ what the downstream received (mock log)"; cat "$WORK/mock.log"
echo; echo "+ gateway log lines for the retries (JSON logs, shortened)"
grep -E 'delivery failed|event delivered|event accepted' "$WORK/gateway.err" "$WORK/gateway.log" 2>/dev/null | sed 's/^[^:]*://' | python3 -c '
import sys, json
for l in sys.stdin:
    try: d = json.loads(l)
    except Exception: continue
    f = d.get("fields", {}); s = d.get("span", {})
    print({k: v for k, v in {"level": d.get("level"), "msg": f.get("message"), "attempt": s.get("attempt"), "retry_in_ms": f.get("retry_in_ms"), "trace_id": s.get("trace_id"), "error": f.get("error")}.items() if v is not None})'

say "7. Dead letter and replay: tenant flaky has a downstream that is not running"
FBODY='{"note":"for the flaky tenant"}'
curl -s -w '\nHTTP %{http_code}\n' -X POST $GW/v1/webhooks -H "X-Api-Key: flaky-api-key" -H "X-Signature: $(sign flaky-secret "$FBODY")" -d "$FBODY"
sleep 4
echo "+ gateway dead-letters"
gateway dead-letters
FID=$(gateway dead-letters | awk '{print $1}' | head -1)
echo; echo "+ start the downstream on that port, then replay $FID"
BIND=127.0.0.1:$DEAD_PORT "$BIN/mock-downstream" > "$WORK/mock2.log" 2>&1 & PIDS+=($!)
sleep 0.5
adm -X POST $GW/admin/events/$FID/replay; echo
sleep 1
echo "+ gateway events --tenant flaky"
gateway events --tenant flaky
echo "+ second mock log"; cat "$WORK/mock2.log"

say "8. Secret rotation with a grace window"
echo "+ gateway rotate-secret acme --grace-secs 600  (secret value hidden here)"
NEW=$(gateway rotate-secret acme --grace-secs 600 | head -1 | sed 's/.*: //')
echo "new secret length: ${#NEW}"
echo "old secret, inside grace window:"
curl -s -o /dev/null -w '  HTTP %{http_code}\n' -X POST $GW/v1/webhooks -H "X-Api-Key: $API_KEY" -H "X-Signature: $(sign $SECRET "$BODY")" -d "$BODY"
echo "(note: the mock downstream still holds the old secret, so these two events fail its signature check, see the metrics)"
echo "new secret:"
curl -s -o /dev/null -w '  HTTP %{http_code}\n' -X POST $GW/v1/webhooks -H "X-Api-Key: $API_KEY" -H "X-Signature: $(sign $NEW "$BODY")" -d "$BODY"

say "9. Admin API needs the token"
curl -s -o /dev/null -w 'no token:   HTTP %{http_code}\n' "$GW/admin/events"
curl -s -o /dev/null -H "Authorization: Bearer $ADMIN" -w 'with token: HTTP %{http_code}\n' "$GW/admin/events?limit=1"

say "10. Metrics"
curl -s $GW/metrics | grep -E '^(gateway_events_received_total|gateway_requests_rejected_total|gateway_deliveries_total|gateway_queue_depth)' | sort

say "11. Graceful shutdown (SIGTERM)"
kill -TERM $GWPID; wait $GWPID 2>/dev/null
grep -E 'shutdown signal|worker stopped|gateway stopped' "$WORK/gateway.err" "$WORK/gateway.log" 2>/dev/null | sed 's/^[^:]*://' | python3 -c '
import sys, json
for l in sys.stdin:
    try: d = json.loads(l)
    except Exception: continue
    print(d["level"], d["fields"]["message"], {k: v for k, v in d["fields"].items() if k != "message"})'
echo; echo "done"
