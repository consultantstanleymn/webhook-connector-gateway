#![allow(dead_code)]

use axum::{
    Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::post,
};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{sync::watch, task::JoinHandle};
use webhook_connector_gateway::{
    app::{Runtime, router},
    config::{Config, RateLimitCfg, TenantCfg},
    now_ms, signature,
    transform::Rule,
};

pub const TENANT: &str = "acme";
pub const API_KEY: &str = "test-api-key";
pub const SECRET: &str = "test-secret";
pub const ADMIN: &str = "admin-token";

#[derive(Clone, Debug)]
pub struct Recorded {
    pub headers: HeaderMap,
    pub body: Vec<u8>,
}

/// A hand-made downstream. It records calls and can fail on demand.
pub struct Mock {
    pub calls: Mutex<Vec<Recorded>>,
    pub call_count: AtomicU64,
    /// Answer 503 for this many calls, then 200.
    pub fail_first: AtomicU64,
    /// When non-zero, always answer with this status.
    pub force_status: AtomicU64,
    pub delay_ms: AtomicU64,
}

impl Mock {
    pub fn recorded(&self) -> Vec<Recorded> {
        self.calls.lock().unwrap().clone()
    }
}

async fn mock_handler(State(m): State<Arc<Mock>>, headers: HeaderMap, body: Bytes) -> StatusCode {
    let n = m.call_count.fetch_add(1, Ordering::SeqCst) + 1;
    let delay = m.delay_ms.load(Ordering::SeqCst);
    if delay > 0 {
        tokio::time::sleep(Duration::from_millis(delay)).await;
    }
    let forced = m.force_status.load(Ordering::SeqCst);
    if forced != 0 {
        return StatusCode::from_u16(forced as u16).unwrap();
    }
    if n <= m.fail_first.load(Ordering::SeqCst) {
        return StatusCode::SERVICE_UNAVAILABLE;
    }
    m.calls.lock().unwrap().push(Recorded {
        headers,
        body: body.to_vec(),
    });
    StatusCode::OK
}

pub async fn start_mock() -> (String, Arc<Mock>) {
    let mock = Arc::new(Mock {
        calls: Mutex::new(vec![]),
        call_count: AtomicU64::new(0),
        fail_first: AtomicU64::new(0),
        force_status: AtomicU64::new(0),
        delay_ms: AtomicU64::new(0),
    });
    let app = Router::new()
        .route("/events", post(mock_handler))
        .with_state(mock.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/events", l.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(l, app).await.unwrap();
    });
    (url, mock)
}

pub struct Harness {
    pub base: String,
    pub mock: Arc<Mock>,
    pub rt: Runtime,
    pub client: reqwest::Client,
    pub shutdown: watch::Sender<bool>,
    pub workers: Vec<JoinHandle<()>>,
    pub server: JoinHandle<()>,
}

/// A directory with one key file named `primary`, shared by all tests of this run.
pub fn test_key_dir() -> String {
    use base64::Engine as _;
    let dir = std::env::temp_dir().join("wcg-test-keys");
    std::fs::create_dir_all(&dir).unwrap();
    for (name, byte) in [("primary", 42u8), ("second", 43u8)] {
        let file = dir.join(name);
        if !file.exists() {
            let key = base64::engine::general_purpose::STANDARD.encode([byte; 32]);
            std::fs::write(&file, key).unwrap();
        }
    }
    dir.display().to_string()
}

/// The database for one test. `GATEWAY_TEST_BACKEND=postgres` uses a fresh schema in
/// the Postgres at `GATEWAY_TEST_PG_URL`. Any other value uses a temp SQLite file.
pub fn test_database_url() -> String {
    if std::env::var("GATEWAY_TEST_BACKEND").as_deref() == Ok("postgres") {
        let base = std::env::var("GATEWAY_TEST_PG_URL")
            .expect("GATEWAY_TEST_PG_URL must be set when GATEWAY_TEST_BACKEND=postgres");
        let schema = format!("gw_{}", uuid::Uuid::new_v4().simple());
        let admin_url = base.clone();
        let schema_for_thread = schema.clone();
        // test_config is not async, so a short-lived runtime on its own thread creates the schema.
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                let pool = sqlx::postgres::PgPoolOptions::new()
                    .max_connections(1)
                    .connect(&admin_url)
                    .await
                    .unwrap();
                // The schema name comes from a UUID made here, never from input.
                sqlx::query(sqlx::AssertSqlSafe(format!(
                    "CREATE SCHEMA {schema_for_thread}"
                )))
                .execute(&pool)
                .await
                .unwrap();
            });
        })
        .join()
        .unwrap();
        let sep = if base.contains('?') { '&' } else { '?' };
        return format!("{base}{sep}options=-c%20search_path%3D{schema}");
    }
    format!(
        "sqlite://{}",
        std::env::temp_dir()
            .join(format!("wcg-it-{}.db", uuid::Uuid::new_v4()))
            .display()
    )
}

#[allow(clippy::field_reassign_with_default)]
pub fn test_config(downstream: &str) -> Config {
    let mut cfg = Config::default();
    cfg.database_url = test_database_url();
    cfg.secrets = webhook_connector_gateway::secrets::SecretsCfg {
        provider: "file".into(),
        dir: Some(test_key_dir()),
        active_kid: "primary".into(),
        previous_kids: vec![],
    };
    cfg.server.admin_token = ADMIN.into();
    cfg.server.max_body_bytes = 4096;
    cfg.queue.workers = 2;
    cfg.queue.max_attempts = 4;
    cfg.queue.backoff_base_ms = 20;
    cfg.queue.backoff_max_ms = 100;
    cfg.queue.poll_ms = 20;
    cfg.delivery.timeout_ms = 2000;
    cfg.delivery.breaker_failures = 1000;
    cfg.tenants.push(TenantCfg {
        id: "acme".into(),
        downstream_url: downstream.into(),
        api_key: Some(API_KEY.into()),
        secret: Some(SECRET.into()),
        rate_limit: RateLimitCfg {
            capacity: 1000.0,
            refill_per_sec: 1000.0,
        },
        rules: vec![
            Rule::Keep {
                field: "type".into(),
                equals: serde_json::json!("order.created"),
            },
            Rule::Rename {
                from: "customer.email".into(),
                to: "email".into(),
            },
            Rule::Enrich {
                field: "source".into(),
                value: serde_json::json!("gateway-test"),
            },
            Rule::Enrich {
                field: "tenant".into(),
                value: serde_json::json!("$tenant_id"),
            },
        ],
    });
    cfg
}

impl Harness {
    pub async fn start() -> Self {
        Self::start_with(|_| {}).await
    }

    pub async fn start_with(tweak: impl FnOnce(&mut Config)) -> Self {
        let (url, mock) = start_mock().await;
        let mut cfg = test_config(&url);
        tweak(&mut cfg);
        let rt = Runtime::build(cfg).await.unwrap();
        let (shutdown, rx) = watch::channel(false);
        let workers = rt.engine.spawn_pool(rx);
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", l.local_addr().unwrap());
        let app = router(rt.state.clone());
        let server = tokio::spawn(async move {
            axum::serve(l, app).await.unwrap();
        });
        Self {
            base,
            mock,
            rt,
            client: reqwest::Client::new(),
            shutdown,
            workers,
            server,
        }
    }

    pub fn sign(&self, body: &str) -> String {
        signature::header_value(SECRET.as_bytes(), now_ms() / 1000, body.as_bytes())
    }

    /// Send a correctly signed webhook.
    pub async fn post(&self, body: &str, idem: Option<&str>) -> reqwest::Response {
        let mut req = self
            .client
            .post(format!("{}/v1/webhooks", self.base))
            .header("x-api-key", API_KEY)
            .header("x-signature", self.sign(body))
            .body(body.to_string());
        if let Some(k) = idem {
            req = req.header("idempotency-key", k);
        }
        req.send().await.unwrap()
    }

    pub async fn status_of(&self, id: &str) -> String {
        self.rt
            .state
            .store
            .get_event(id)
            .await
            .unwrap()
            .unwrap()
            .summary
            .status
    }

    /// Wait until the event has the status, or fail after 10 seconds.
    pub async fn wait_status(&self, id: &str, want: &str) {
        for _ in 0..500 {
            if self.status_of(id).await == want {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!(
            "event {id} never reached {want}, is {}",
            self.status_of(id).await
        );
    }

    pub async fn admin_get(&self, path: &str) -> reqwest::Response {
        self.client
            .get(format!("{}{}", self.base, path))
            .bearer_auth(ADMIN)
            .send()
            .await
            .unwrap()
    }
}

pub const ORDER: &str =
    r#"{"type":"order.created","order_id":7,"customer":{"email":"a@b.c","id":3}}"#;
