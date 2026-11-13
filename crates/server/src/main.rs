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
        #[arg(long, env = "WETUBE_STATIC_DIR", default_value = "dist")]
        static_dir: PathBuf,
        #[arg(long, env = "WETUBE_INCOMING_DIR")]
        incoming_dir: Option<PathBuf>,
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
            static_dir,
            incoming_dir,
        } => {
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
            let app =
                wetube_server::router_with_incoming(pool, &origin, &static_dir, incoming).await?;
            let listener = tokio::net::TcpListener::bind(bind)
                .await
                .context("bind HTTP listener")?;
            println!("WeTube listening on {bind}; configured origin: {origin}");
            axum::serve(listener, app)
                .with_graceful_shutdown(shutdown())
                .await?;
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
