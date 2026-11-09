//! Configuration, loaded from a TOML file with a few env overrides.

use crate::{ratelimit::Limit, transform::Rule};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ServerCfg {
    pub bind: String,
    pub max_body_bytes: usize,
    /// Bearer token for `/admin`. Empty means the admin API is off.
    pub admin_token: String,
    /// Accepted clock difference for signatures, in seconds.
    pub signature_tolerance_secs: i64,
    /// How long the old secret stays valid after a rotation, in seconds.
    pub rotation_grace_secs: i64,
}

impl Default for ServerCfg {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:8080".into(),
            max_body_bytes: 256 * 1024,
            admin_token: String::new(),
            signature_tolerance_secs: 300,
            rotation_grace_secs: 86_400,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct QueueCfg {
    pub workers: usize,
    pub max_attempts: u32,
    pub backoff_base_ms: u64,
    pub backoff_max_ms: u64,
    pub poll_ms: u64,
    /// How long a claimed event is reserved for one worker.
    pub lease_ms: i64,
    pub shutdown_grace_secs: u64,
    /// Ingest answers 503 when pending plus processing events reach this number.
    pub max_pending: i64,
    /// The Retry-After value for that 503, in seconds.
    pub retry_after_secs: u64,
}

impl Default for QueueCfg {
    fn default() -> Self {
        Self {
            workers: 4,
            max_attempts: 6,
            backoff_base_ms: 1000,
            backoff_max_ms: 300_000,
            poll_ms: 500,
            lease_ms: 60_000,
            shutdown_grace_secs: 20,
            max_pending: 50_000,
            retry_after_secs: 5,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct DeliveryCfg {
    pub timeout_ms: u64,
    pub breaker_failures: u32,
    pub breaker_cooldown_ms: u64,
    /// Retries may be at most this share of first attempts in the window.
    pub retry_budget_ratio: f64,
    /// Retries that are always allowed in the window.
    pub retry_budget_min: u32,
    pub retry_budget_window_ms: i64,
}

impl Default for DeliveryCfg {
    fn default() -> Self {
        Self {
            timeout_ms: 10_000,
            breaker_failures: 5,
            breaker_cooldown_ms: 30_000,
            retry_budget_ratio: 0.2,
            retry_budget_min: 10,
            retry_budget_window_ms: 60_000,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default)]
pub struct RateLimitCfg {
    pub capacity: f64,
    pub refill_per_sec: f64,
}

impl Default for RateLimitCfg {
    fn default() -> Self {
        Self {
            capacity: 100.0,
            refill_per_sec: 50.0,
        }
    }
}

impl From<RateLimitCfg> for Limit {
    fn from(c: RateLimitCfg) -> Self {
        Limit {
            capacity: c.capacity,
            refill_per_sec: c.refill_per_sec,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct TenantCfg {
    pub id: String,
    pub downstream_url: String,
    /// Optional. When missing the gateway makes one and prints it once.
    pub api_key: Option<String>,
    /// Optional. Same rule as `api_key`.
    pub secret: Option<String>,
    #[serde(default)]
    pub rate_limit: RateLimitCfg,
    #[serde(default)]
    pub rules: Vec<Rule>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    pub database_url: String,
    pub server: ServerCfg,
    pub queue: QueueCfg,
    pub delivery: DeliveryCfg,
    pub tenants: Vec<TenantCfg>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            database_url: "sqlite://gateway.db".into(),
            server: ServerCfg::default(),
            queue: QueueCfg::default(),
            delivery: DeliveryCfg::default(),
            tenants: Vec::new(),
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("cannot read config {}: {e}", path.display()))?;
        let mut cfg: Config = toml::from_str(&text)
            .map_err(|e| anyhow::anyhow!("bad config {}: {e}", path.display()))?;
        if let Ok(v) = std::env::var("GATEWAY_DATABASE_URL") {
            cfg.database_url = v;
        }
        if let Ok(v) = std::env::var("GATEWAY_ADMIN_TOKEN") {
            cfg.server.admin_token = v;
        }
        if let Ok(v) = std::env::var("GATEWAY_BIND") {
            cfg.server.bind = v;
        }
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        let mut seen = HashMap::new();
        for t in &self.tenants {
            if t.id.is_empty()
                || !t
                    .id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            {
                anyhow::bail!("tenant id {:?} must use letters, digits, - and _", t.id);
            }
            if seen.insert(&t.id, ()).is_some() {
                anyhow::bail!("tenant {} is listed twice", t.id);
            }
            if !(t.downstream_url.starts_with("http://")
                || t.downstream_url.starts_with("https://"))
            {
                anyhow::bail!("tenant {}: downstream_url must be http or https", t.id);
            }
        }
        if self.queue.workers == 0 || self.queue.max_attempts == 0 {
            anyhow::bail!("queue.workers and queue.max_attempts must be above 0");
        }
        Ok(())
    }

    pub fn tenant(&self, id: &str) -> Option<&TenantCfg> {
        self.tenants.iter().find(|t| t.id == id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_example() {
        let cfg: Config = toml::from_str(
            r#"
            database_url = "sqlite://x.db"
            [queue]
            workers = 2
            [[tenants]]
            id = "acme"
            downstream_url = "http://localhost:9000/events"
            rate_limit = { capacity = 5.0, refill_per_sec = 1.0 }
            rules = [{ op = "rename", from = "a", to = "b" }]
            "#,
        )
        .unwrap();
        cfg.validate().unwrap();
        assert_eq!(cfg.queue.workers, 2);
        assert_eq!(cfg.queue.max_attempts, 6);
        assert_eq!(cfg.tenant("acme").unwrap().rules.len(), 1);
    }

    #[test]
    fn rejects_bad_tenants() {
        let mut cfg = Config::default();
        cfg.tenants.push(TenantCfg {
            id: "bad id".into(),
            downstream_url: "http://x".into(),
            api_key: None,
            secret: None,
            rate_limit: RateLimitCfg::default(),
            rules: vec![],
        });
        assert!(cfg.validate().is_err());
    }
}
