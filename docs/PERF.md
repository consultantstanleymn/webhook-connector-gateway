# Performance

These numbers are modest and come from one old laptop that was also doing other work. Do not read them as what the design can do on a server.

## Machine

- MacBook Pro, mid 2015, running Fedora Linux 44 (kernel 7.2)
- Intel Core i7-4770HQ, 4 cores and 8 threads, 2.2 GHz base
- 16 GB RAM, SSD, SQLite file on the encrypted btrfs root
- Other programs were running. The load average was about 4.5 on 8 threads before each run.
- The load tool, the gateway (4 workers), and the mock downstream all ran on this one machine. The load tool takes CPU from the gateway.
- Release build (`--release`, thin LTO). Logging at `warn`.

## What is measured

`src/bin/loadtest.rs` sends signed events to `POST /v1/webhooks` with 32 concurrent clients. Each event has a unique Idempotency-Key. Time is measured from request start to the response (202). That is the time the customer sees: check the key, check the signature, save to SQL, answer. Delivery to the downstream is separate and runs in the worker pool at the same time.

Reproduce: `cargo build --release && scripts/perf.sh`

## Results (final code)

3 runs of 5,000 requests each, 32 concurrent clients, no failures (15,000 of 15,000 answered 202):

| Run | Throughput | p50 | p95 | p99 | max |
|---|---|---|---|---|---|
| 1 | 4,411 req/s | 6.15 ms | 9.91 ms | 24.84 ms | 134.5 ms |
| 2 | 4,091 req/s | 6.33 ms | 10.82 ms | 25.37 ms | 185.3 ms |
| 3 | 4,128 req/s | 6.43 ms | 10.91 ms | 24.87 ms | 184.3 ms |

So about 4,100 to 4,400 accepted events per second, with p99 near 25 ms, while the workers delivered events to the mock downstream at the same time.

Queue drain: 13,414 events were still waiting when the last run ended. With no new traffic the 4 workers delivered them in 6 s, about 2,200 events/s. All 15,000 reached the mock downstream (`calls: 15000, ok: 15000`).

Raw output: see `scripts/perf.sh`. The run above printed:

```text
load average before: 4.83 4.68 3.73
--- run 1
requests:     5000
concurrency:  32
accepted 202: 5000
other:        0
duration:     1.13 s
throughput:   4411 req/s
latency ms:   p50=6.15 p95=9.91 p99=24.84 max=134.50
--- run 2
requests:     5000
concurrency:  32
accepted 202: 5000
other:        0
duration:     1.22 s
throughput:   4091 req/s
latency ms:   p50=6.33 p95=10.82 p99=25.37 max=185.29
--- run 3
requests:     5000
concurrency:  32
accepted 202: 5000
other:        0
duration:     1.21 s
throughput:   4128 req/s
latency ms:   p50=6.43 p95=10.91 p99=24.87 max=184.30
--- waiting for the workers to drain the queue
13414 events were still queued when the last run ended; drained in 6 s (2162 events/s with no new traffic), left: 0
--- queue after the runs
gateway_queue_depth{status="dead"} 0
gateway_queue_depth{status="delivered"} 15000
gateway_queue_depth{status="filtered"} 0
gateway_queue_depth{status="pending"} 0
gateway_queue_depth{status="processing"} 0
downstream saw: {"calls":15000,"ok":15000}
```

## A bug the load test found

My first run gave 736 to 1,087 req/s, p99 70 to 150 ms, and the queue drained at only about 150 events/s. The cause was the query that takes the next event. It used one `OR` over two conditions and an `ORDER BY`. With a backlog, SQLite had to collect every due row and sort it for each claim, so the cost grew with the queue length. `EXPLAIN QUERY PLAN` showed `USE TEMP B-TREE FOR ORDER BY`.

The fix: two small queries (due events first, then expired leases) and two partial indexes (`WHERE status = 'pending'` and `WHERE status = 'processing'`). Each claim now reads one index entry in order. Same machine, same test: about 4x the accept rate and about 14x the drain rate.

| | Accept rate | p99 | Drain rate |
|---|---|---|---|
| Before the fix (run 1 / 2 / 3) | 845 / 1,087 / 736 req/s | 70 / 112 / 152 ms | about 150 events/s |
| After the fix (run 1 / 2 / 3) | 4,411 / 4,091 / 4,128 req/s | 24.8 / 25.4 / 24.9 ms | about 2,200 events/s |

The "before" numbers were taken from the first perf run, which had no drain measurement. The drain figure for it is an estimate: 3,297 events delivered about 22 seconds after the first run started.

## Limits of these numbers

- One machine, three runs, no warm-up control. The spread between runs is about 8%.
- The max latency (130 to 185 ms) comes from SQLite write contention and checkpoints. The max in the first, slow runs was over 3 seconds.
- SQLite allows one writer at a time. Each accepted event is one write, each delivery is one claim write and one result write. The ceiling for this design on SQLite is the write rate of the disk and the lock. A server with Postgres would behave differently. That was not measured.
- The mock downstream answers at once. A real downstream with 100 ms latency would lower the drain rate with 4 workers (roughly 4 / 0.1 = 40 events/s), so raise `queue.workers` for slow downstreams.
- `synchronous=NORMAL` is used (see DECISIONS.md). `FULL` would be slower.
- The k6 tool is not used. The load tool is a Rust binary in this repo.

## Drain math and backpressure

The gateway refuses new events with 503 and `Retry-After` when pending plus processing events reach `queue.max_pending` (default 50,000). This section shows how to pick that limit.

With W workers, a mean delivery time T seconds and a downstream success share p, the drain rate is about:

```
drain rate = W * p / T   events per second
```

If events arrive at rate A and A is below the drain rate, the backlog B shrinks. The time to drain it is:

```
time to drain = B / (drain rate - A)
```

If A is above the drain rate, the backlog grows without bound. Backpressure then protects the database and the memory of the host.

Worked example, using the numbers above (measured on one laptop, load tool and mock on the same machine): the 4 workers drained about 2,200 events per second with a fast mock. Take p = 1 and no arrivals. A backlog of 50,000 events then drains in about 50,000 / 2,200 = 23 seconds. Real ERP downstreams are slower. If T is 0.2 s, then W = 4 gives 4 / 0.2 = 20 events per second, and the same backlog needs about 2,500 seconds (42 minutes). Choose `max_pending` from the longest wait that the customer accepts: `max_pending = drain rate * accepted wait`. The 2,200 events per second figure comes from the fast mock, so it is an upper bound, not a forecast for a real ERP.

`Retry-After` tells senders when to try again. The default is 5 seconds (`queue.retry_after_secs`). The counter `gateway_ingest_shed_total{reason="queue_full"}` counts refused requests.

## k6 and the Rust load tool, side by side

Run on 2026-11-09 with `scripts/k6-ingest.sh`. Release build, SQLite store, 4 workers, mock downstream. The load generators, the gateway and the mock ran on the same laptop (MacBook Pro mid 2015). Numbers are measured on one laptop. Other programs ran at the same time (load average about 2). Treat them as a rough guide, not as a capacity forecast.

| Tool | Setting | Accepted | Failed | p50 | p95 | Notes |
|---|---|---|---|---|---|---|
| k6 | constant arrival rate 500 req/s, 20 s | 10,001 | 0.00 % | 0.36 ms | 0.47 ms | no dropped iterations |
| k6 | constant arrival rate 2,000 req/s, 20 s | 39,414 | 0.00 % | 0.68 ms | 48.3 ms | 587 iterations dropped (k6 could not keep the rate) |
| Rust `loadtest` | 5,000 requests, concurrency 32 | 5,000 | 0 | 4.91 ms | 13.83 ms | 3,393 req/s, p99 125 ms |

The two tools measure the same endpoint in different ways. k6 holds an arrival rate (open model). The Rust tool holds a number of concurrent clients (closed model). At 2,000 req/s k6 reports about 1,969 req/s, so the machine was near its limit with the load generator on board. Both agree that ingest handles a few thousand requests per second on this laptop, which is about the figure that the earlier runs above found (about 4,100 to 4,400 req/s from the Rust tool alone). The queue was still draining at the end: 4,446 events were pending when the script read the metrics.
