#!/usr/bin/env bash
# Send one signed event through the gateway with span export on, then fetch the trace
# from a local Jaeger. Jaeger UI on 25686, OTLP gRPC on 25617, gateway on 25680, mock on 25690.
# Usage (HEAVY, needs a Jaeger image): flock heavy.lock nice -n 10 scripts/trace-run.sh <out.json>
set -u
cd "$(dirname "$0")/.." || exit 1
OUT="${1:-/tmp/rgw-trace.json}"
T=/tmp/rgw-trace
JAEGER=rgw-jaeger
BIN=target/debug
rm -rf "$T"; mkdir -p "$T"
nice -n 10 cargo build -j 2 --locked --bins >/dev/null 2>&1 || { echo "build failed"; exit 1; }
podman rm -f "$JAEGER" >/dev/null 2>&1 || true
podman run -d --name "$JAEGER" -p 127.0.0.1:25686:16686 -p 127.0.0.1:25617:4317 \
  docker.io/jaegertracing/all-in-one:1.66.0 >/dev/null || { echo "jaeger failed to start"; exit 1; }
cat > "$T/gateway.toml" <<TOML
database_url = "sqlite://$T/gateway.db"
[server]
bind = "127.0.0.1:25680"
[[tenants]]
id = "trace"
api_key = "trace-key"
secret = "trace-secret"
downstream_url = "http://127.0.0.1:25690/events"
TOML
PIDS=()
# shellcheck disable=SC2329  # called by the EXIT trap
cleanup() { for p in "${PIDS[@]}"; do kill "$p" 2>/dev/null; done; podman rm -f "$JAEGER" >/dev/null 2>&1; }
trap cleanup EXIT
for _ in $(seq 1 60); do curl -sf http://127.0.0.1:25686/ >/dev/null && break; sleep 1; done
BIND=127.0.0.1:25690 "$BIN/mock-downstream" > "$T/mock.log" 2>&1 & PIDS+=($!)
OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:25617 "$BIN/gateway" --config "$T/gateway.toml" serve > "$T/gateway.log" 2>&1 & GW=$!; PIDS+=("$GW")
for _ in $(seq 1 60); do curl -sf http://127.0.0.1:25680/readyz >/dev/null && break; sleep 1; done
BODY='{"type":"job.closed","job_id":"J-1001"}'
SIG=$(printf '%s' "$BODY" | WEBHOOK_SECRET=trace-secret "$BIN/gateway" --config "$T/gateway.toml" sign --body-file - )
echo "post status: $(curl -s -o /dev/null -w '%{http_code}' -X POST http://127.0.0.1:25680/v1/webhooks \
  -H 'X-Api-Key: trace-key' -H "X-Signature: $SIG" -H 'Content-Type: application/json' -d "$BODY")"
sleep 3
echo "mock stats: $(curl -s http://127.0.0.1:25690/stats)"
# Stop the gateway with SIGTERM so it flushes the last spans.
kill -TERM "$GW"; wait "$GW" 2>/dev/null
sleep 3
curl -s "http://127.0.0.1:25686/api/traces?service=webhook-connector-gateway&limit=5" > "$OUT"
echo "span names in the trace: $(jq -r '[.data[].spans[].operationName] | unique | join(", ")' "$OUT")"
echo "trace count: $(jq '.data | length' "$OUT")"
echo "spans in the largest trace: $(jq '[.data[] | .spans | length] | max' "$OUT")"
