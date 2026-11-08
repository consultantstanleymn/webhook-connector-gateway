//! Load test for the inbound endpoint.
//!
//! Sends signed, unique events with a fixed number of concurrent clients and
//! prints requests per second and latency percentiles.
//!
//! Usage: loadtest --url http://127.0.0.1:8080 --api-key K --secret S
//!                 [--requests 5000] [--concurrency 32]

use clap::Parser;
use std::{sync::Arc, time::Instant};
use webhook_connector_gateway::{now_ms, signature};

#[derive(Parser)]
struct Args {
    #[arg(long, default_value = "http://127.0.0.1:8080")]
    url: String,
    #[arg(long, env = "LOAD_API_KEY")]
    api_key: String,
    #[arg(long, env = "LOAD_SECRET", hide_env_values = true)]
    secret: String,
    #[arg(long, default_value_t = 5000)]
    requests: usize,
    #[arg(long, default_value_t = 32)]
    concurrency: usize,
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((sorted.len() as f64 * p).ceil() as usize).clamp(1, sorted.len()) - 1;
    sorted[idx]
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let a = Arc::new(Args::parse());
    let client = reqwest::Client::builder()
        .pool_max_idle_per_host(a.concurrency)
        .build()?;
    let run_id = uuid::Uuid::new_v4().simple().to_string();
    let next = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let start = Instant::now();

    let mut tasks = Vec::new();
    for _ in 0..a.concurrency {
        let (a, client, next, run_id) = (a.clone(), client.clone(), next.clone(), run_id.clone());
        tasks.push(tokio::spawn(async move {
            let mut lat = Vec::new();
            let (mut accepted, mut other) = (0usize, 0usize);
            loop {
                let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if i >= a.requests {
                    break;
                }
                let body = format!(r#"{{"type":"order.created","order_id":{i},"amount":12.5,"customer":{{"email":"load@example.com"}}}}"#);
                let sig = signature::header_value(a.secret.as_bytes(), now_ms() / 1000, body.as_bytes());
                let t = Instant::now();
                let res = client
                    .post(format!("{}/v1/webhooks", a.url))
                    .header("x-api-key", &a.api_key)
                    .header("x-signature", sig)
                    .header("idempotency-key", format!("{run_id}-{i}"))
                    .header("content-type", "application/json")
                    .body(body)
                    .send()
                    .await;
                lat.push(t.elapsed().as_secs_f64() * 1000.0);
                match res {
                    Ok(r) if r.status().as_u16() == 202 => accepted += 1,
                    _ => other += 1,
                }
            }
            (lat, accepted, other)
        }));
    }
    let mut lat = Vec::new();
    let (mut accepted, mut other) = (0, 0);
    for t in tasks {
        let (l, ok, bad) = t.await?;
        lat.extend(l);
        accepted += ok;
        other += bad;
    }
    let secs = start.elapsed().as_secs_f64();
    lat.sort_by(|x, y| x.total_cmp(y));
    println!("requests:     {}", a.requests);
    println!("concurrency:  {}", a.concurrency);
    println!("accepted 202: {accepted}");
    println!("other:        {other}");
    println!("duration:     {secs:.2} s");
    println!("throughput:   {:.0} req/s", a.requests as f64 / secs);
    println!(
        "latency ms:   p50={:.2} p95={:.2} p99={:.2} max={:.2}",
        percentile(&lat, 0.50),
        percentile(&lat, 0.95),
        percentile(&lat, 0.99),
        lat.last().copied().unwrap_or(0.0)
    );
    Ok(())
}
