#!/usr/bin/env bash
# Run the full test suite on SQLite, then on Postgres. Starts and stops the Postgres container by exact name.
# Usage: scripts/test-both-backends.sh   (HEAVY: wrap with the heavy lock and nice)
set -u
cd "$(dirname "$0")/.." || exit 1
NAME=rgw-pg-test
PORT=25632
export GATEWAY_TEST_PG_URL="postgres://gw:gw@127.0.0.1:${PORT}/gw_test"
echo "uptime start: $(uptime)"
started=0
if ! podman ps --format '{{.Names}}' | grep -qx "$NAME"; then
  podman rm -f "$NAME" >/dev/null 2>&1 || true
  podman run -d --name "$NAME" -p "127.0.0.1:${PORT}:5432" -e POSTGRES_USER=gw -e POSTGRES_PASSWORD=gw \
    -e POSTGRES_DB=gw_test docker.io/library/postgres:16-alpine >/dev/null
  started=1
fi
for _ in $(seq 1 30); do podman exec "$NAME" pg_isready -U gw >/dev/null 2>&1 && break; sleep 2; done
rc=0
echo "=== run 1: backend sqlite"
GATEWAY_TEST_BACKEND=sqlite nice -n 10 cargo test -j 2 --locked 2>&1 | grep -E "^(     Running|test result)" || rc=1
[ "${PIPESTATUS[0]}" = 0 ] || rc=1
echo "=== run 2: backend postgres"
GATEWAY_TEST_BACKEND=postgres nice -n 10 cargo test -j 2 --locked 2>&1 | grep -E "^(     Running|test result)" || rc=1
[ "${PIPESTATUS[0]}" = 0 ] || rc=1
[ "$started" = 1 ] && podman rm -f "$NAME" >/dev/null 2>&1
echo "uptime end: $(uptime)"
echo "exit=$rc"
exit "$rc"
