# Service level objective: delivery

Targets for a fictional customer. No production data.

## The SLO

99 percent of accepted events are delivered within 60 seconds, while the downstream is up. The window is 28 days.

## The indicator

The gateway records `gateway_delivery_latency_seconds`: the time from accepting an event to its delivery.

```
sum(rate(gateway_delivery_latency_seconds_bucket{le="60"}[5m]))
/
sum(rate(gateway_delivery_latency_seconds_count[5m]))
```

The recording rules `gateway:delivery_within_60s:ratio_rate5m`, `30m`, `1h` and `6h` hold this ratio for four windows.

## Why "while the downstream is up"

If the customer ERP is down, the gateway cannot deliver. The gateway retries and keeps the event. An alert at that time does not help the gateway owner. The alerts check that at least 90 percent of delivery attempts in the last 10 minutes succeeded:

```
sum(rate(gateway_deliveries_total{outcome="delivered"}[10m]))
/ sum(rate(gateway_deliveries_total{outcome=~"delivered|retry|dead"}[10m])) > 0.9
```

A downstream outage has its own signals: the circuit breaker, the retry budget counter and the dead-letter list.

## Alerts

| Alert | Windows | Burn factor | Severity |
|---|---|---|---|
| `GatewayDeliveryBurnFast` | 1 h and 5 m | 14.4 | page |
| `GatewayDeliveryBurnSlow` | 6 h and 30 m | 6 | ticket |

`promtool test rules` proves three cases for each alert: it fires on a burn while the downstream is up, it stays quiet on normal traffic, and it stays quiet on a burn while the downstream is down. See `evidence/promtool-delivery-slo.txt`.

## Error budget

The budget is 1 percent of events that may be slower than 60 seconds in 28 days. At half of the budget spent, review the cause. At the full budget, stop risky changes until the owner reviews.

## Limits

The rules are tested on synthetic series. The metric has bounded labels (none). Retries add to the delivery time, so a slow downstream shows in this SLO even when it is up.
