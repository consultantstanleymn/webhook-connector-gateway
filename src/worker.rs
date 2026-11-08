//! Worker pool: claim an event, transform it, deliver it, record the result.

use crate::{
    backoff::BackoffPolicy,
    breaker::Breakers,
    config::Config,
    delivery::{Deliverer, DeliveryResult},
    metrics::Metrics,
    now_ms,
    store::{Claimed, Store},
    transform::{self, Outcome},
};
use rand::RngExt;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    sync::{Notify, watch},
    task::JoinHandle,
};
use tracing::{Instrument, info, warn};

#[derive(Clone)]
pub struct Engine {
    pub store: Store,
    pub cfg: Arc<Config>,
    pub deliverer: Deliverer,
    pub breakers: Arc<Breakers>,
    pub metrics: Metrics,
    pub notify: Arc<Notify>,
}

impl Engine {
    pub fn new(
        store: Store,
        cfg: Arc<Config>,
        metrics: Metrics,
        notify: Arc<Notify>,
    ) -> anyhow::Result<Self> {
        let deliverer = Deliverer::new(Duration::from_millis(cfg.delivery.timeout_ms))?;
        let breakers = Arc::new(Breakers::new(
            cfg.delivery.breaker_failures,
            Duration::from_millis(cfg.delivery.breaker_cooldown_ms),
        ));
        Ok(Self {
            store,
            cfg,
            deliverer,
            breakers,
            metrics,
            notify,
        })
    }

    fn policy(&self) -> BackoffPolicy {
        BackoffPolicy {
            base_ms: self.cfg.queue.backoff_base_ms,
            max_ms: self.cfg.queue.backoff_max_ms,
        }
    }

    /// Handle one due event. Returns false when the queue had nothing due.
    pub async fn process_one(&self) -> anyhow::Result<bool> {
        let Some(ev) = self.store.claim(now_ms(), self.cfg.queue.lease_ms).await? else {
            return Ok(false);
        };
        let span = tracing::info_span!("event", event_id = %ev.id, tenant = %ev.tenant_id,
                                        trace_id = %ev.trace_id, attempt = ev.attempts);
        self.handle(ev).instrument(span).await?;
        Ok(true)
    }

    async fn handle(&self, ev: Claimed) -> anyhow::Result<()> {
        let Some(tenant) = self.cfg.tenant(&ev.tenant_id) else {
            self.store
                .dead_letter(&ev.id, "tenant is not in the config")
                .await?;
            self.metrics.deliveries.with_label_values(&["dead"]).inc();
            return Ok(());
        };
        let Some(creds) = self.store.tenant_by_id(&ev.tenant_id).await? else {
            self.store
                .dead_letter(&ev.id, "tenant has no credentials")
                .await?;
            return Ok(());
        };
        let doc: serde_json::Value = match serde_json::from_str(&ev.payload) {
            Ok(v) => v,
            Err(e) => {
                self.store
                    .dead_letter(&ev.id, &format!("stored payload is not JSON: {e}"))
                    .await?;
                return Ok(());
            }
        };
        let ctx = transform::Context {
            tenant_id: &ev.tenant_id,
            event_id: &ev.id,
        };
        let out = match transform::apply(&tenant.rules, &ctx, doc) {
            Outcome::Deliver(v) => v,
            Outcome::Filtered => {
                self.store.mark_filtered(&ev.id).await?;
                self.metrics
                    .deliveries
                    .with_label_values(&["filtered"])
                    .inc();
                info!("event filtered by rules");
                return Ok(());
            }
        };

        let breaker = self.breakers.for_tenant(&ev.tenant_id);
        if let Err(wait) = breaker.acquire(Instant::now()) {
            self.store
                .release(&ev.id, now_ms() + wait.as_millis() as i64 + 1)
                .await?;
            self.metrics
                .deliveries
                .with_label_values(&["circuit_open"])
                .inc();
            warn!(
                wait_ms = wait.as_millis() as u64,
                "circuit open, delivery postponed"
            );
            return Ok(());
        }

        let started = Instant::now();
        let result = self
            .deliverer
            .deliver(
                &tenant.downstream_url,
                &creds.secret,
                &ev.id,
                &ev.trace_id,
                serde_json::to_vec(&out)?,
            )
            .await;
        let secs = started.elapsed().as_secs_f64();

        match result {
            DeliveryResult::Success => {
                breaker.on_success();
                self.store.mark_delivered(&ev.id).await?;
                self.observe("delivered", secs);
                info!("event delivered");
            }
            DeliveryResult::Permanent(err) => {
                breaker.on_success(); // the downstream answered, so it is up
                self.store.dead_letter(&ev.id, &err).await?;
                self.observe("permanent_failure", secs);
                warn!(error = %err, "permanent failure, event dead-lettered");
            }
            DeliveryResult::Retryable(err) => {
                breaker.on_failure(Instant::now());
                if ev.attempts >= self.cfg.queue.max_attempts as i64 {
                    self.store
                        .dead_letter(&ev.id, &format!("max attempts reached: {err}"))
                        .await?;
                    self.observe("dead", secs);
                    warn!(error = %err, "max attempts reached, event dead-lettered");
                } else {
                    let delay = self
                        .policy()
                        .delay_ms(ev.attempts as u32, rand::rng().random::<f64>());
                    self.store
                        .schedule_retry(&ev.id, now_ms() + delay as i64, &err)
                        .await?;
                    self.observe("retry", secs);
                    warn!(error = %err, retry_in_ms = delay, "delivery failed, will retry");
                }
            }
        }
        Ok(())
    }

    fn observe(&self, outcome: &str, secs: f64) {
        self.metrics.deliveries.with_label_values(&[outcome]).inc();
        self.metrics
            .delivery_duration
            .with_label_values(&[outcome])
            .observe(secs);
    }

    /// Start the workers. They stop when `shutdown` turns true, after the
    /// event they hold is done.
    pub fn spawn_pool(&self, shutdown: watch::Receiver<bool>) -> Vec<JoinHandle<()>> {
        (0..self.cfg.queue.workers)
            .map(|n| {
                let engine = self.clone();
                let mut shutdown = shutdown.clone();
                tokio::spawn(async move {
                    let poll = Duration::from_millis(engine.cfg.queue.poll_ms);
                    while !*shutdown.borrow() {
                        match engine.process_one().await {
                            Ok(true) => {}
                            Ok(false) => {
                                tokio::select! {
                                    _ = engine.notify.notified() => {}
                                    _ = tokio::time::sleep(poll) => {}
                                    _ = shutdown.changed() => {}
                                }
                            }
                            Err(e) => {
                                warn!(worker = n, error = %e, "worker error");
                                tokio::time::sleep(Duration::from_secs(1)).await;
                            }
                        }
                    }
                    info!(worker = n, "worker stopped");
                })
            })
            .collect()
    }
}
