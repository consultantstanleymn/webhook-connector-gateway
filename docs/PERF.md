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
