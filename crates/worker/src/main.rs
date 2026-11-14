use anyhow::Result;
use clap::{Parser, Subcommand};
use std::{path::PathBuf, sync::Arc, time::Duration};
use wetube_backend::{
    db,
    lock::{MaintenanceLock, ServiceLock},
};

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
    #[command(hide = true)]
    ScanMetadata,
    /// Print media-tool capabilities without accessing any ISO or database.
    Doctor,
    /// Inspect one completed ISO copy without conversion, publication, or deletion.
    InspectDvd {
        #[arg(long)]
        iso: PathBuf,
        #[arg(long, value_parser = clap::value_parser!(u8).range(1..=99))]
        title: u8,
        #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u8).range(1..=9))]
        angle: u8,
        /// Confirm the copy has finished; this does not prove exclusion of SMB writers.
        #[arg(long, required = true)]
        copy_complete: bool,
    },
    /// Process durable inspection, conversion, and local publication jobs. Never deletes sources.
    Run {
        #[arg(long)]
        once: bool,
        #[arg(long, env = "WETUBE_INCOMING_DIR", requires = "scratch_dir")]
        incoming_dir: Option<PathBuf>,
        #[arg(long, env = "WETUBE_SCRATCH_DIR", requires = "incoming_dir")]
        scratch_dir: Option<PathBuf>,
        #[arg(long, env = "WETUBE_MEDIA_DIR", requires = "scratch_dir")]
        media_dir: Option<PathBuf>,
        #[arg(long, default_value_t = 14400, value_parser = clap::value_parser!(u64).range(1..=86400))]
        tool_timeout_secs: u64,
        #[arg(long, default_value_t = 2, value_parser = clap::value_parser!(u8).range(1..=16))]
        threads: u8,
        #[arg(long, default_value_t = 8 * 1024 * 1024 * 1024u64)]
        max_output_bytes: u64,
        #[arg(long, env = "WETUBE_MAX_ISO_BYTES", default_value_t = 16 * 1024 * 1024 * 1024u64)]
        max_iso_bytes: u64,
        #[arg(long, env = "WETUBE_MAX_SCRATCH_BYTES", default_value_t = 32 * 1024 * 1024 * 1024u64)]
        max_scratch_bytes: u64,
        #[arg(long, env = "WETUBE_STAGING_TIMEOUT_SECS", default_value_t = 600, value_parser = clap::value_parser!(u64).range(1..=3600))]
        staging_timeout_secs: u64,
    },
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let cli = Cli::parse();
    if matches!(cli.command, Command::ScanMetadata) {
        // The scanner is single-threaded here, before any async runtime exists.
        unsafe {
            std::env::set_var("DVDCSS_CACHE", "off");
        }
        let report = wetube_worker::dvdnav::scan_stdin()?;
        unsafe {
            libc::fflush(std::ptr::null_mut());
        }
        println!(
            "\nWETUBE_DVD_SCAN_JSON\n{}",
            serde_json::to_string(&report)?
        );
        return Ok(());
    }
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run(cli))
}

async fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Command::ScanMetadata => unreachable!("scanner does not initialize async runtime"),
        Command::Doctor => {
            let report = wetube_worker::diagnose().await;
            println!("{}", serde_json::to_string_pretty(&report)?);
            if !report.required_tools_available() {
                std::process::exit(1);
            }
        }
        Command::InspectDvd {
            iso,
            title,
            angle,
            copy_complete: _,
        } => {
            let inspection = wetube_worker::probe::inspect_dvd(&iso, title, angle);
            tokio::select! {
                report = inspection => println!("{}", serde_json::to_string_pretty(&report?)?),
                _ = shutdown() => anyhow::bail!("DVD inspection interrupted; input retained"),
            }
        }
        Command::Run {
            once,
            incoming_dir,
            scratch_dir,
            media_dir,
            tool_timeout_secs,
            threads,
            max_output_bytes,
            max_iso_bytes,
            max_scratch_bytes,
            staging_timeout_secs,
        } => {
            let _services = ServiceLock::acquire(&cli.state_dir, false)?;
            let _lock = MaintenanceLock::acquire(&cli.state_dir)?;
            let pool = db::connect(&cli.state_dir).await?;
            db::check_version(&pool).await?;
            let media = media_dir
                .as_deref()
                .map(wetube_backend::preview::PreviewRoot::open)
                .transpose()?;
            if let Some(media) = &media {
                media.require_separate(&cli.state_dir)?;
                if let Some(incoming) = &incoming_dir {
                    media.require_separate(incoming)?;
                }
                if let Some(scratch) = &scratch_dir {
                    media.require_separate(scratch)?;
                }
            }
            let limits = wetube_worker::conversions::Limits {
                timeout: Duration::from_secs(tool_timeout_secs),
                threads,
                max_output_bytes,
            };
            limits.validate()?;
            let imports = match (incoming_dir, scratch_dir) {
                (Some(incoming), Some(scratch)) => {
                    Some(Arc::new(wetube_worker::imports::ImportConfig::open(
                        &incoming,
                        &scratch,
                        max_iso_bytes,
                        max_scratch_bytes,
                        Duration::from_secs(staging_timeout_secs),
                    )?))
                }
                (None, None) => None,
                _ => anyhow::bail!("incoming and scratch directories must be configured together"),
            };
            let _scratch_lock = imports
                .as_ref()
                .map(|config| {
                    config.require_separate(&cli.state_dir)?;
                    config.acquire_scratch_lock()
                })
                .transpose()?;
            wetube_worker::recover(&pool).await?;
            wetube_worker::imports::recover(&pool).await?;
            wetube_worker::conversions::recover(&pool).await?;
            wetube_worker::scans::recover(&pool).await?;
            let mut interval = tokio::time::interval(Duration::from_secs(2));
            let shutdown = shutdown();
            tokio::pin!(shutdown);
            let mut stopping = false;
            loop {
                tokio::select! {
                    _ = &mut shutdown => break,
                    _ = interval.tick() => {}
                }
                let work = async {
                    wetube_worker::tick(&pool).await?;
                    if let Some(config) = &imports {
                        wetube_worker::scans::tick(&pool, config.clone()).await?;
                        wetube_worker::imports::tick(&pool, config.clone()).await?;
                        if let Some(media) = &media {
                            wetube_worker::conversions::tick(&pool, config.clone(), media, &limits)
                                .await?;
                        }
                    }
                    anyhow::Ok(())
                };
                tokio::pin!(work);
                let mut heartbeat = tokio::time::interval(Duration::from_secs(2));
                loop {
                    tokio::select! {
                        result = &mut work => { result?; break; },
                        _ = &mut shutdown, if !stopping => {
                            stopping = true;
                            tracing::info!("finishing current work before releasing worker ownership");
                        },
                        _ = heartbeat.tick() => {
                            if let Err(error) = wetube_worker::heartbeat(&pool).await {
                                tracing::warn!(%error, "unable to update worker heartbeat");
                            }
                        }
                    }
                }
                if once || stopping {
                    break;
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
