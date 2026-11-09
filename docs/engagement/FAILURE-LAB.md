# Failure lab: kill -9 and redrive

Question: after the gateway is killed hard during delivery, and every dead letter is redriven, does the downstream apply each event once?

## Setup

`scripts/failure-lab-redrive.sh [sqlite|postgres]` runs on local ports: gateway 25680, mock downstream 25690.

1. The mock downstream fails 60 percent of calls and applies each `Idempotency-Key` once.
2. The gateway runs with `max_attempts = 3` and short backoff. It sends 500 signed events from the `loadtest` tool.
3. The script waits until some events are dead-lettered, then runs `kill -9` on the gateway.
4. The mock fail rate drops to 0 (through `POST /fail-rate/0.0`, so the mock keeps its counts). The gateway restarts.
5. The script runs `gateway replay --all`, waits for an empty queue, and reads the mock counts.

The pass rule is `unique == 500` at the mock and 500 events with status `delivered` in the admin API.

## Result

| Backend | Events | Applied once (unique) | Duplicate applications | Dead letters replayed | Result |
|---|---|---|---|---|---|
| SQLite | 500 | 500 | 0 | 4 | PASS |
| Postgres 16 | 500 | 500 | 0 | 2 | PASS |

Evidence: `evidence/failure-lab-redrive-sqlite.txt` and `evidence/failure-lab-redrive-postgres.txt`.

## What this proves, and what it does not

- Delivery is at least once. A killed gateway leaves events in `processing`. The lease runs out and another worker claims them again.
- The mock removes repeats by key, so the count of applied events equals the count of events. In these runs no event was applied twice (duplicates 0). A run with a different kill time could produce duplicate deliveries. The downstream must dedupe on `Idempotency-Key`.
- The kill happens early because the load tool sends the events fast. The run is short and uses debug builds. It runs on one laptop.
- The lab does not prove exactly-once delivery. The gateway does not claim it.
