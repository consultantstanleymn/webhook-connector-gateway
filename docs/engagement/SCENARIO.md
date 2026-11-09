Fictional customer. Any match with a real company is not intended.

# Scenario: FieldServiceCo

## Customer

FieldServiceCo makes field-service software. Technicians report job events: job started, parts used, job closed. Each event must reach up to three customer ERP systems. The ERPs are slow and sometimes go down.

## The question

"Will each event arrive once, and how will we know when it does not?"

## What the gateway does

The gateway accepts signed webhooks, stores each event in a durable queue, and delivers it to the tenant ERP with retries. A circuit breaker, a rate limit and a dead-letter list protect the ERPs and the queue.

## In scope

- A Postgres store next to SQLite, with versioned migrations.
- Global backpressure and a per-tenant retry budget.
- A failure lab that kills the gateway during delivery and then redrives dead letters.
- One delivery SLO with a burn-rate alert.
- Load tests, trace export, encrypted signing secrets, an image scan.

## Out of scope

- Exactly-once delivery. The gateway delivers at least once. The downstream removes repeats with the idempotency key.
- Distributed rate limiting across many gateway instances.
- A cloud key management service.
- Any real customer system. All runs use a local mock downstream.
