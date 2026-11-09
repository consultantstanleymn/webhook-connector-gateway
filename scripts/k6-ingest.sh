#!/usr/bin/env bash
# Measure the inbound endpoint with k6 at two arrival rates, then with the Rust load tool.
# Gateway on 25680, mock downstream on 25690. Everything runs on one machine, so the
# numbers are for that machine only. Usage (HEAVY): flock heavy.lock nice -n 10 scripts/k6-ingest.sh
set -u
export GATEWAY_SECRET_PRIMARY="${GATEWAY_SECRET_PRIMARY:-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=}"  # demo key for local runs, not a secret
cd "$(dirname "$0")/.." || exit 1
BIN=target/release
T=/tmp/rgw-k6
rm -rf "$T"; mkdir -p "$T"
echo "uptime start: $(uptime)"
nice -n 10 cargo build --release -j 2 --locked --bins >/dev/null 2>&1 || { echo "release build failed"; exit 1; }
cat > "$T/gateway.toml" <<TOML
database_url = "sqlite://$T/gateway.db"
[server]
bind = "127.0.0.1:25680"
[queue]
workers = 4
[[tenants]]
id = "load"
api_key = "load-key"
secret = "load-secret"
downstream_url = "http://127.0.0.1:25690/events"
rate_limit = { capacity = 1000000.0, refill_per_sec = 1000000.0 }
rules = [{ op = "enrich", field = "source", value = "loadtest" }]
TOML
PIDS=()
# shellcheck disable=SC2329  # called by the EXIT trap
cleanup() { for p in "${PIDS[@]}"; do kill "$p" 2>/dev/null; done; wait 2>/dev/null; }
trap cleanup EXIT
BIND=127.0.0.1:25690 "$BIN/mock-downstream" > /dev/null 2>&1 & PIDS+=($!)
RUST_LOG=warn "$BIN/gateway" --config "$T/gateway.toml" serve > /dev/null 2>&1 & PIDS+=($!)
for _ in $(seq 100); do curl -sf http://127.0.0.1:25680/readyz >/dev/null && break; sleep 0.1; done
for rate in 500 2000; do
  echo "=== k6 constant arrival rate $rate req/s for 20 s"
  nice -n 10 k6 run -e RATE=$rate -e DURATION=20s load/ingest.js 2>&1 | grep -E "^ *(http_req_duration|http_req_failed|http_reqs|dropped_iterations|iterations)[ .]|✓|✗"
done
sleep 5
echo "=== Rust load tool: 5000 requests, concurrency 32"
"$BIN/loadtest" --url http://127.0.0.1:25680 --api-key load-key --secret load-secret --requests 5000 --concurrency 32
echo "=== queue after the runs"
curl -s http://127.0.0.1:25680/metrics | grep -E '^gateway_queue_depth'
echo "uptime end: $(uptime)"
