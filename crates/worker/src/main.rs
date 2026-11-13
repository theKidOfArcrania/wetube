use anyhow::Result;
use clap::{Parser, Subcommand};
use std::{path::PathBuf, time::Duration};
use wetube_backend::{db, lock::MaintenanceLock};

#[derive(Parser)]
#[command(version, about = "WeTube non-destructive diagnostic worker")]
struct Cli {
    #[arg(
        long,
        env = "WETUBE_STATE_DIR",
        default_value = ".wetube",
        global = true
    )]
    state_dir: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Print media-tool capabilities without accessing any ISO or database.
    Doctor,
    /// Process durable diagnostic jobs. No conversion or input deletion exists.
    Run {
        #[arg(long)]
        once: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let cli = Cli::parse();
    match cli.command {
        Command::Doctor => {
            let report = wetube_worker::diagnose().await;
            println!("{}", serde_json::to_string_pretty(&report)?);
            if report.tools.iter().any(|tool| !tool.available) {
                std::process::exit(1);
            }
        }
        Command::Run { once } => {
            let _lock = MaintenanceLock::acquire(&cli.state_dir)?;
            let pool = db::connect(&cli.state_dir).await?;
            db::check_version(&pool).await?;
            wetube_worker::recover(&pool).await?;
            if once {
                wetube_worker::tick(&pool).await?;
            } else {
                let mut interval = tokio::time::interval(Duration::from_secs(2));
                let shutdown = shutdown();
                tokio::pin!(shutdown);
                loop {
                    tokio::select! {
                        _ = &mut shutdown => break,
                        _ = interval.tick() => { wetube_worker::tick(&pool).await?; }
                    }
                }
            }
            pool.close().await;
        }
    }
    Ok(())
}

async fn shutdown() {
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("install SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {},
        _ = terminate.recv() => {},
    }
}
