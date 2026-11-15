use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::{
    io::{self, BufRead, Read},
    net::SocketAddr,
    os::unix::fs::DirBuilderExt,
    path::PathBuf,
};
use wetube_backend::{
    accounts, db,
    lock::{MaintenanceLock, ServiceLock},
};
use wetube_core::Role;

#[derive(Parser)]
#[command(version, about = "WeTube server and local account administration")]
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
    /// Initialize or upgrade local state. Stop both services first.
    Init,
    /// Create an account without passing a password on the command line.
    AddUser {
        #[arg(long)]
        username: String,
        #[arg(long)]
        admin: bool,
        #[arg(long)]
        password_stdin: bool,
    },
    /// Reset a password and revoke all sessions for that account.
    ResetPassword {
        #[arg(long)]
        username: String,
        #[arg(long)]
        password_stdin: bool,
    },
    Serve {
        #[arg(long, env = "WETUBE_BIND", default_value = "127.0.0.1:8080")]
        bind: SocketAddr,
        #[arg(long, env = "WETUBE_ORIGIN", default_value = "http://localhost:8080")]
        origin: String,
        /// Accept any Host header; configured Origin, authentication and CSRF checks stay enforced.
        #[arg(long, env = "WETUBE_ALLOW_ANY_HOST")]
        allow_any_host: bool,
        #[arg(long, env = "WETUBE_STATIC_DIR", default_value = "dist")]
        static_dir: PathBuf,
        #[arg(long, env = "WETUBE_INCOMING_DIR")]
        incoming_dir: Option<PathBuf>,
        #[arg(long, env = "WETUBE_MEDIA_DIR")]
        media_dir: Option<PathBuf>,
        #[arg(long, env = "WETUBE_SCRATCH_DIR", requires = "media_dir")]
        scratch_dir: Option<PathBuf>,
        /// Server-only TMDB API Read Access Token; never pass the token itself.
        #[arg(long, env = "WETUBE_TMDB_TOKEN_FILE", requires = "tmdb_accept_terms")]
        tmdb_token_file: Option<PathBuf>,
        /// Operator accepts the current TMDB terms and attribution/retention duties.
        #[arg(long, env = "WETUBE_TMDB_ACCEPT_TERMS")]
        tmdb_accept_terms: bool,
    },
    /// Admit an operator-confirmed experimental MP4 for LOCAL preview; stop services first.
    AdmitPreview {
        #[arg(long)]
        report: PathBuf,
        #[arg(long, env = "WETUBE_MEDIA_DIR")]
        media_dir: PathBuf,
        #[arg(long)]
        title: String,
        #[arg(long)]
        year: Option<i32>,
        #[arg(long, default_value = "")]
        description: String,
        /// Operator-confirmed digest; experimental reports do not contain payload hashes.
        #[arg(long)]
        movie_sha256: String,
        #[arg(long, default_value_t = 8 * 1024 * 1024 * 1024u64)]
        max_bytes: u64,
        #[arg(long, default_value_t = 14400)]
        timeout_secs: u64,
        /// Additional incoming root to enforce separation, if configured.
        #[arg(long, env = "WETUBE_INCOMING_DIR")]
        incoming_dir: Option<PathBuf>,
        #[arg(long, env = "WETUBE_STATIC_DIR", default_value = "dist")]
        static_dir: PathBuf,
    },
}

fn read_password(stdin: bool) -> Result<String> {
    if stdin {
        let mut password = String::new();
        let bytes = io::stdin().lock().take(1027).read_line(&mut password)?;
        anyhow::ensure!(bytes > 0 && bytes <= 1026, "invalid password input");
        if password.ends_with('\n') {
            password.pop();
            if password.ends_with('\r') {
                password.pop();
            }
        }
        Ok(password)
    } else {
        Ok(rpassword::prompt_password("Password: ")?)
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let cli = Cli::parse();
    match cli.command {
        Command::Init => {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&cli.state_dir)?;
            let _services = ServiceLock::acquire(&cli.state_dir, true)?;
            let _lock = MaintenanceLock::acquire(&cli.state_dir)?;
            db::initialize(&cli.state_dir).await?;
            println!("Initialized WeTube state.");
        }
        Command::AddUser {
            username,
            admin,
            password_stdin,
        } => {
            let _services = ServiceLock::acquire(&cli.state_dir, true)?;
            let _lock = MaintenanceLock::acquire(&cli.state_dir)?;
            let pool = db::connect(&cli.state_dir).await?;
            db::check_version(&pool).await?;
            accounts::create_user(
                &pool,
                &username,
                read_password(password_stdin)?,
                if admin { Role::Admin } else { Role::Viewer },
            )
            .await?;
            println!("Account created.");
        }
        Command::ResetPassword {
            username,
            password_stdin,
        } => {
            let _services = ServiceLock::acquire(&cli.state_dir, true)?;
            let _lock = MaintenanceLock::acquire(&cli.state_dir)?;
            let pool = db::connect(&cli.state_dir).await?;
            db::check_version(&pool).await?;
            accounts::reset_password(&pool, &username, read_password(password_stdin)?).await?;
            println!("Password reset; existing sessions revoked.");
        }
        Command::Serve {
            bind,
            origin,
            allow_any_host,
            static_dir,
            incoming_dir,
            media_dir,
            scratch_dir,
            tmdb_token_file,
            tmdb_accept_terms,
        } => {
            anyhow::ensure!(
                tmdb_token_file.is_none() || tmdb_accept_terms,
                "TMDB requires explicit operator acceptance of its current terms"
            );
            let _services = ServiceLock::acquire(&cli.state_dir, false)?;
            anyhow::ensure!(
                static_dir.join("index.html").is_file(),
                "frontend is not built; run trunk build --release in crates/web first"
            );
            let pool = db::connect(&cli.state_dir).await?;
            let incoming = incoming_dir
                .as_deref()
                .map(wetube_backend::incoming::IncomingRoot::open)
                .transpose()?;
            let media = media_dir
                .as_deref()
                .map(wetube_backend::preview::PreviewRoot::open)
                .transpose()?;
            let mut app = wetube_server::router_with_host_policy(
                pool.clone(),
                &origin,
                &static_dir,
                incoming,
                media,
                scratch_dir.as_deref(),
                allow_any_host,
            )
            .await?;
            if let Some(path) = tmdb_token_file {
                app = app.layer(axum::Extension(
                    wetube_server::metadata::Tmdb::from_token_file(&path)?,
                ));
            }
            wetube_backend::metadata::expire(&pool, wetube_backend::now()).await?;
            let listener = tokio::net::TcpListener::bind(bind)
                .await
                .context("bind HTTP listener")?;
            println!("WeTube listening on {bind}; configured origin: {origin}");
            let maintenance = tokio::spawn(async move {
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
                loop {
                    interval.tick().await;
                    if let Err(error) =
                        wetube_backend::metadata::expire(&pool, wetube_backend::now()).await
                    {
                        tracing::error!(%error, "metadata expiry cleanup failed");
                    }
                }
            });
            let result = axum::serve(listener, app)
                .with_graceful_shutdown(shutdown())
                .await;
            maintenance.abort();
            let _ = maintenance.await;
            result?;
        }
        Command::AdmitPreview {
            report,
            media_dir,
            title,
            year,
            description,
            movie_sha256,
            max_bytes,
            timeout_secs,
            incoming_dir,
            static_dir,
        } => {
            let _services = ServiceLock::acquire(&cli.state_dir, true)?;
            let _lock = MaintenanceLock::acquire(&cli.state_dir)?;
            let pool = db::connect(&cli.state_dir).await?;
            db::check_version(&pool).await?;
            let root = wetube_backend::preview::PreviewRoot::open(&media_dir)?;
            root.require_separate(&cli.state_dir)?;
            if static_dir.exists() {
                root.require_separate(&static_dir)?;
            }
            if let Some(incoming) = incoming_dir {
                root.require_separate(&incoming)?;
            }
            let admission = wetube_server::admission::admit(
                &pool,
                &root,
                wetube_server::admission::Admission {
                    report: &report,
                    title: &title,
                    year,
                    description: &description,
                    movie_sha256: &movie_sha256,
                    max_bytes,
                    timeout: std::time::Duration::from_secs(timeout_secs),
                },
            )
            .await?;
            println!("{}", serde_json::to_string(&admission)?);
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
