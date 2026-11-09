#!/usr/bin/env bash
# Failure lab: kill -9 the gateway during delivery, then redrive every dead letter.
# The mock downstream applies each Idempotency-Key once. The lab checks that every event
# was applied once: unique == total, with the counts read from the mock.
# Usage: scripts/failure-lab-redrive.sh [sqlite|postgres]   (default sqlite)
#   postgres needs GATEWAY_TEST_PG_URL and a container named rgw-pg-test (psql inside it).
# Ports: gateway 25680, mock downstream 25690. Debug builds are enough for this lab.
set -u
export GATEWAY_SECRET_PRIMARY="${GATEWAY_SECRET_PRIMARY:-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=}"  # demo key for local runs, not a secret
cd "$(dirname "$0")/.." || exit 1
BACKEND="${1:-sqlite}"
TOTAL="${TOTAL:-500}"
T=/tmp/rgw-lab-$BACKEND
GW=127.0.0.1:25680
MOCK=127.0.0.1:25690
ADMIN=lab-admin
BIN=target/debug
rm -rf "$T"; mkdir -p "$T"
echo "backend: $BACKEND, events: $TOTAL"
echo "uptime start: $(uptime)"

nice -n 10 cargo build -j 2 --locked --bins >/dev/null 2>&1 || { echo "build failed"; exit 1; }

if [ "$BACKEND" = postgres ]; then
  schema="lab_$(date +%s)"
  podman exec rgw-pg-test psql -U gw -d gw_test -c "CREATE SCHEMA $schema" >/dev/null
  DB="${GATEWAY_TEST_PG_URL}?options=-c%20search_path%3D$schema"
else
  DB="sqlite://$T/lab.db"
fi

cat > "$T/gateway.toml" <<TOML
database_url = "$DB"
[server]
bind = "$GW"
admin_token = "$ADMIN"
[queue]
workers = 4
max_attempts = 3
backoff_base_ms = 50
backoff_max_ms = 200
poll_ms = 50
lease_ms = 2000
[delivery]
timeout_ms = 2000
breaker_failures = 1000000
breaker_cooldown_ms = 1000
retry_budget_min = 1000000
[[tenants]]
id = "lab"
api_key = "lab-key"
secret = "lab-secret"
downstream_url = "http://$MOCK/events"
rate_limit = { capacity = 100000.0, refill_per_sec = 100000.0 }
TOML
export GATEWAY_CONFIG="$T/gateway.toml"

MOCK_PID=""; GW_PID=""
# shellcheck disable=SC2329  # called by the EXIT trap
cleanup() { [ -n "$GW_PID" ] && kill "$GW_PID" 2>/dev/null; [ -n "$MOCK_PID" ] && kill "$MOCK_PID" 2>/dev/null; :; }
trap cleanup EXIT

BIND=$MOCK FAIL_RATE=0.6 STATS_FILE="$T/mock-stats.json" "$BIN/mock-downstream" > "$T/mock.log" 2>&1 &
MOCK_PID=$!
"$BIN/gateway" serve > "$T/gateway1.log" 2>&1 &
GW_PID=$!
for _ in $(seq 1 60); do curl -sf "http://$GW/readyz" >/dev/null && break; sleep 1; done

echo "--- send $TOTAL signed events while the mock fails 60 percent of calls"
"$BIN/loadtest" --url "http://$GW" --api-key lab-key --secret lab-secret --requests "$TOTAL" --concurrency 8 | grep -E "requests|throughput" 

count_status() { curl -s -H "Authorization: Bearer $ADMIN" "http://$GW/admin/events?status=$1&limit=1000" | jq '.events | length'; }
for _ in $(seq 1 100); do
  d=$(count_status dead); [ "${d:-0}" -gt 0 ] && break; sleep 0.1
done
echo "dead letters before the kill: $(count_status dead), still pending or processing: $(( $(count_status pending) + $(count_status processing) ))"
echo "--- kill -9 the gateway (pid $GW_PID) during delivery"
kill -9 "$GW_PID"; wait "$GW_PID" 2>/dev/null; GW_PID=""
echo "mock stats right after the kill: $(curl -s "http://$MOCK/stats")"

echo "--- the downstream recovers (fail rate 0), the gateway restarts"
curl -s -X POST "http://$MOCK/fail-rate/0.0" >/dev/null
"$BIN/gateway" serve > "$T/gateway2.log" 2>&1 &
GW_PID=$!
for _ in $(seq 1 60); do curl -sf "http://$GW/readyz" >/dev/null && break; sleep 1; done
sleep 4   # leases from the killed process (2 s) run out and those events are claimed again

echo "--- redrive every dead letter: gateway replay --all"
"$BIN/gateway" replay --all
for _ in $(seq 1 120); do
  open=$(( $(count_status pending) + $(count_status processing) ))
  [ "$open" -eq 0 ] && break; sleep 0.5
done
echo "dead letters after redrive: $(count_status dead), open events: $open"
echo "events with status delivered (admin API): $(count_status delivered)"
stats=$(curl -s "http://$MOCK/stats")
echo "mock stats: $stats"
unique=$(echo "$stats" | jq .unique); dups=$(echo "$stats" | jq .duplicates)
echo "applied at the downstream: unique=$unique of $TOTAL, duplicate applications=$dups"
echo "uptime end: $(uptime)"
if [ "$unique" = "$TOTAL" ] && [ "$(count_status delivered)" = "$TOTAL" ]; then
  echo "RESULT: PASS (every event applied once by key)"; exit 0
fi
echo "RESULT: FAIL"; exit 1
