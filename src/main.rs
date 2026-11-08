use anyhow::Context;
use clap::{Parser, Subcommand};
use std::{path::PathBuf, time::Duration};
use tokio::sync::watch;
use webhook_connector_gateway::{
    admin,
    app::{Runtime, router},
    config::Config,
    now_ms, observability, signature,
    store::Store,
};

#[derive(Parser)]
#[command(name = "gateway", version, about = "Webhook and connector gateway")]
struct Cli {
    /// Path to the TOML config file.
    #[arg(
        long,
        short,
        env = "GATEWAY_CONFIG",
        default_value = "gateway.toml",
        global = true
    )]
    config: PathBuf,
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the HTTP server and the worker pool.
    Serve,
    /// List events.
    Events {
        #[arg(long)]
        status: Option<String>,
        #[arg(long)]
        tenant: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: i64,
    },
    /// Show one event with its payload.
    Show { id: String },
    /// List dead letters.
    DeadLetters {
        #[arg(long, default_value_t = 50)]
        limit: i64,
    },
    /// Put a dead event back in the queue.
    Replay {
        /// Event id. Leave out with --all.
        id: Option<String>,
        /// Replay every dead event.
        #[arg(long, conflicts_with = "id")]
        all: bool,
    },
    /// Make a new secret for a tenant. Prints it once.
    RotateSecret {
        tenant: String,
        /// Seconds the old secret stays valid.
        #[arg(long)]
        grace_secs: Option<i64>,
    },
    /// Print a signature header for a body. Useful for tests and onboarding.
    Sign {
        #[arg(long, env = "WEBHOOK_SECRET", hide_env_values = true)]
        secret: String,
        /// File with the body, or - for stdin.
        #[arg(long)]
        body_file: PathBuf,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    if let Cmd::Sign { secret, body_file } = &cli.command {
        let body = if body_file.as_os_str() == "-" {
            let mut b = Vec::new();
            std::io::Read::read_to_end(&mut std::io::stdin(), &mut b)?;
            b
        } else {
            std::fs::read(body_file)
                .with_context(|| format!("cannot read {}", body_file.display()))?
        };
        println!(
            "{}",
            signature::header_value(secret.as_bytes(), now_ms() / 1000, &body)
        );
        return Ok(());
    }

    let cfg = Config::load(&cli.config)?;
    match cli.command {
        Cmd::Serve => serve(cfg).await,
        Cmd::Sign { .. } => unreachable!("handled above"),
        other => admin_command(other, cfg).await,
    }
}

async fn serve(cfg: Config) -> anyhow::Result<()> {
    observability::init_logging();
    let grace = Duration::from_secs(cfg.queue.shutdown_grace_secs);
    let bind = cfg.server.bind.clone();
    let rt = Runtime::build(cfg).await?;
    for c in &rt.created {
        // Printed to stdout once, not logged. The secret is not stored in the clear in logs.
        println!(
            "tenant {} created\n  api_key: {}\n  secret:  {}",
            c.tenant_id, c.api_key, c.secret
        );
    }

    let (tx, rx) = watch::channel(false);
    let workers = rt.engine.spawn_pool(rx);
    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .with_context(|| format!("cannot bind {bind}"))?;
    tracing::info!(addr = %listener.local_addr()?, workers = workers.len(), "gateway listening");

    let tx2 = tx.clone();
    axum::serve(listener, router(rt.state))
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            tracing::info!("shutdown signal received, draining");
            let _ = tx2.send(true);
        })
        .await?;

    // HTTP is closed. Let workers finish the event they hold.
    let _ = tx.send(true);
    let join_all = async {
        for w in workers {
            let _ = w.await;
        }
    };
    if tokio::time::timeout(grace, join_all).await.is_err() {
        tracing::warn!(
            "workers did not stop in time; leased events will be retried after the lease ends"
        );
    }
    tracing::info!("gateway stopped");
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! { _ = ctrl_c => {}, _ = term => {} }
}

async fn admin_command(cmd: Cmd, cfg: Config) -> anyhow::Result<()> {
    let store = Store::connect(&cfg.database_url).await?;
    match cmd {
        Cmd::Events {
            status,
            tenant,
            limit,
        } => {
            println!(
                "{:<36}  {:<10} {:<10} {:>3}  {}",
                "ID", "TENANT", "STATUS", "TRY", "LAST ERROR"
            );
            for e in store
                .list_events(status.as_deref(), tenant.as_deref(), limit)
                .await?
            {
                println!(
                    "{:<36}  {:<10} {:<10} {:>3}  {}",
                    e.id,
                    e.tenant_id,
                    e.status,
                    e.attempts,
                    e.last_error.unwrap_or_default()
                );
            }
        }
        Cmd::Show { id } => match store.get_event(&id).await? {
            Some(e) => println!("{}", serde_json::to_string_pretty(&e)?),
            None => anyhow::bail!("event {id} not found"),
        },
        Cmd::DeadLetters { limit } => {
            for d in store.list_dead_letters(limit).await? {
                println!(
                    "{}  tenant={} attempts={} replayed={} error={}",
                    d.event_id,
                    d.tenant_id,
                    d.attempts,
                    d.replayed_at.is_some(),
                    d.last_error
                );
            }
        }
        Cmd::Replay { id, all } => {
            if all {
                println!("replayed {} events", store.replay_all_dead().await?);
            } else {
                let id = id.context("give an event id or --all")?;
                if !store.replay(&id).await? {
                    anyhow::bail!("event {id} is not a dead letter");
                }
                println!("replayed {id}");
            }
        }
        Cmd::RotateSecret { tenant, grace_secs } => {
            let grace = grace_secs.unwrap_or(cfg.server.rotation_grace_secs);
            let secret = admin::rotate_secret(&store, &tenant, grace)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            println!(
                "new secret for {tenant}: {secret}\nold secret stays valid for {grace} seconds"
            );
        }
        Cmd::Serve | Cmd::Sign { .. } => unreachable!("handled in main"),
    }
    Ok(())
}
