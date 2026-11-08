#!/usr/bin/env bash
# Measure the inbound endpoint. Run from the repo root after `cargo build --release`.
# Starts the gateway and a mock downstream on random free ports. The load
# client runs on the same machine, so numbers are for that machine only.
set -u
BIN=${BIN:-target/release}
REQUESTS=${REQUESTS:-5000}
CONCURRENCY=${CONCURRENCY:-32}
RUNS=${RUNS:-3}
WORK=$(mktemp -d)
free_port() { python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1]);s.close()'; }
GW_PORT=$(free_port); MOCK_PORT=$(free_port)
cat > "$WORK/gateway.toml" <<CFG
database_url = "sqlite://$WORK/gateway.db"
[server]
bind = "127.0.0.1:$GW_PORT"
[queue]
workers = 4
[[tenants]]
id = "load"
api_key = "load-key"
secret = "load-secret"
downstream_url = "http://127.0.0.1:$MOCK_PORT/events"
rate_limit = { capacity = 1000000.0, refill_per_sec = 1000000.0 }
rules = [{ op = "enrich", field = "source", value = "loadtest" }]
CFG
PIDS=(); trap 'for p in "${PIDS[@]}"; do kill $p 2>/dev/null; done; wait 2>/dev/null' EXIT
BIND=127.0.0.1:$MOCK_PORT "$BIN/mock-downstream" > /dev/null 2>&1 & PIDS+=($!)
RUST_LOG=warn "$BIN/gateway" --config "$WORK/gateway.toml" serve > /dev/null 2>&1 & PIDS+=($!)
for _ in $(seq 100); do curl -sf http://127.0.0.1:$GW_PORT/readyz >/dev/null && break; sleep 0.1; done
echo "load average before: $(cut -d' ' -f1-3 /proc/loadavg)"
for i in $(seq $RUNS); do
  echo "--- run $i"
  "$BIN/loadtest" --url http://127.0.0.1:$GW_PORT --api-key load-key --secret load-secret \
    --requests $REQUESTS --concurrency $CONCURRENCY
done
echo "--- waiting for the workers to drain the queue"
depth() { curl -s http://127.0.0.1:$GW_PORT/metrics | awk '/^gateway_queue_depth\{status="(pending|processing)"\}/ {n+=$2} END {print n+0}'; }
P0=$(depth); D0=$(date +%s.%N); P=$P0
for _ in $(seq 900); do P=$(depth); [ "$P" = "0" ] && break; sleep 1; done
D1=$(date +%s.%N)
echo "$P0 events were still queued when the last run ended; drained in $(echo "$D1 $D0" | awk '{printf "%.0f", $1-$2}') s ($(echo "$D1 $D0 $P0" | awk '{printf "%.0f", $3/($1-$2)}') events/s with no new traffic), left: $P"
echo "--- queue after the runs"
curl -s http://127.0.0.1:$GW_PORT/metrics | grep -E '^gateway_queue_depth'
echo "downstream saw: $(curl -s http://127.0.0.1:$MOCK_PORT/stats)"
