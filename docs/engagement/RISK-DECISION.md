# Risk decision: per-process limits

| Field | Value |
|---|---|
| Risk | The circuit breaker, the rate limit and the retry budget keep their state in one process. With several gateway instances, each instance counts alone. The real limits can be several times higher than the settings. |
| Options | Accept. Mitigate with a shared counter store (for example Redis). Transfer to a gateway in front of the service. |
| Decision | Accept: breaker, rate limit and retry budget state is per process. Multi-instance limits are approximate. |
| Owner | Stanley Sujith Nelavala |
| Review date | 2027-02-01 |

## Compensating controls

- Global backpressure uses the shared database count, so the queue limit is exact across instances (within the 250 ms cache).
- The retry budget and breaker are conservative by default.
- Metrics show per-instance behavior. Alerts use sums over all instances.

## Condition to revisit

Review when the gateway runs with more than one instance, or when a customer needs an exact per-tenant limit.
