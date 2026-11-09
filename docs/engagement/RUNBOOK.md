# Runbook additions

The base procedures are in [`docs/RUNBOOK.md`](../RUNBOOK.md). This file adds entries for the new controls.

## Backpressure (503 queue_full)

Senders get 503 with `Retry-After`. The counter `gateway_ingest_shed_total{reason="queue_full"}` rises.

1. Check `gateway_queue_depth` and the delivery rate. See the drain math in `docs/PERF.md`.
2. If a downstream is down, the queue fills. Check the breaker and the dead-letter list.
3. Raise `queue.max_pending` only if the host has room. Add workers if the downstream can take more load.

## Retry budget exhausted

The counter `gateway_retry_budget_exhausted_total{tenant}` rises. Retries wait for the longest backoff.

1. The downstream of that tenant fails often. Check its health first.
2. The events are not lost. They retry slowly and move to the dead-letter list after `max_attempts`.
3. Do not raise the budget during an outage. It adds load to a system that is already hurt.

## delivery-slo-burn

Alert `GatewayDeliveryBurnFast` or `GatewayDeliveryBurnSlow`. More than 1 percent of events take more than 60 seconds while the downstream is up.

1. Check `gateway_delivery_latency_seconds` and the queue depth.
2. A long queue means too few workers or slow deliveries. Add workers or fix the slow downstream.
3. Check the retry budget counter. Events that wait for the longest backoff are slow by design.

## Redrive

1. List dead letters: `gateway dead-letters`.
2. Fix the cause (downstream, rules, tenant config).
3. Run `gateway replay --all`, or `gateway replay <id>`.
4. The downstream must dedupe on `Idempotency-Key`. The failure lab shows one applied effect for each event.

## Key reseal

See section 3b of [`docs/RUNBOOK.md`](../RUNBOOK.md). The command is `gateway reseal-secrets --to <kid>`.
