//! Prometheus metrics.

use prometheus::{
    CounterVec, Encoder, HistogramOpts, HistogramVec, IntGaugeVec, Opts, Registry, TextEncoder,
};

#[derive(Clone)]
pub struct Metrics {
    registry: Registry,
    pub events_received: CounterVec,
    pub requests_rejected: CounterVec,
    pub deliveries: CounterVec,
    pub http_duration: HistogramVec,
    pub delivery_duration: HistogramVec,
    pub queue_depth: IntGaugeVec,
}

const BUCKETS: &[f64] = &[
    0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

impl Metrics {
    pub fn new() -> Self {
        let registry = Registry::new();
        let events_received = CounterVec::new(
            Opts::new(
                "gateway_events_received_total",
                "Events accepted into the queue",
            ),
            &["tenant"],
        )
        .expect("valid metric");
        let requests_rejected = CounterVec::new(
            Opts::new(
                "gateway_requests_rejected_total",
                "Inbound requests refused",
            ),
            &["reason"],
        )
        .expect("valid metric");
        let deliveries = CounterVec::new(
            Opts::new("gateway_deliveries_total", "Delivery attempts by outcome"),
            &["outcome"],
        )
        .expect("valid metric");
        let http_duration = HistogramVec::new(
            HistogramOpts::new(
                "gateway_http_request_duration_seconds",
                "HTTP request latency",
            )
            .buckets(BUCKETS.to_vec()),
            &["route", "method", "status"],
        )
        .expect("valid metric");
        let delivery_duration = HistogramVec::new(
            HistogramOpts::new(
                "gateway_delivery_duration_seconds",
                "Downstream call latency",
            )
            .buckets(BUCKETS.to_vec()),
            &["outcome"],
        )
        .expect("valid metric");
        let queue_depth = IntGaugeVec::new(
            Opts::new("gateway_queue_depth", "Events by status"),
            &["status"],
        )
        .expect("valid metric");
        for c in [
            Box::new(events_received.clone()) as Box<dyn prometheus::core::Collector>,
            Box::new(requests_rejected.clone()),
            Box::new(deliveries.clone()),
            Box::new(http_duration.clone()),
            Box::new(delivery_duration.clone()),
            Box::new(queue_depth.clone()),
        ] {
            registry.register(c).expect("register once");
        }
        Self {
            registry,
            events_received,
            requests_rejected,
            deliveries,
            http_duration,
            delivery_duration,
            queue_depth,
        }
    }

    /// Set the depth gauge for every known status, so empty ones read 0.
    pub fn set_depth(&self, rows: &[(String, i64)]) {
        for status in ["pending", "processing", "delivered", "filtered", "dead"] {
            let n = rows
                .iter()
                .find(|(s, _)| s == status)
                .map(|(_, n)| *n)
                .unwrap_or(0);
            self.queue_depth.with_label_values(&[status]).set(n);
        }
    }

    pub fn render(&self) -> String {
        let mut buf = Vec::new();
        if TextEncoder::new()
            .encode(&self.registry.gather(), &mut buf)
            .is_err()
        {
            return String::new();
        }
        String::from_utf8(buf).unwrap_or_default()
    }
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}
