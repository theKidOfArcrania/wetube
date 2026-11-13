use anyhow::{Context, Result, bail};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{Row, SqlitePool};
use std::path::Path;
use std::time::Duration;
use wetube_core::{APP_VERSION, SCHEMA_VERSION};

pub async fn connect(state_dir: &Path) -> Result<SqlitePool> {
    open(state_dir, false).await
}

async fn open(state_dir: &Path, create: bool) -> Result<SqlitePool> {
    let options = SqliteConnectOptions::new()
        .filename(state_dir.join("wetube.sqlite3"))
        .create_if_missing(create)
        .foreign_keys(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Full)
        .busy_timeout(Duration::from_secs(5));
    SqlitePoolOptions::new()
        .max_connections(4)
        .connect_with(options)
        .await
        .context("open local SQLite state; run wetube-server init first")
}

pub async fn initialize(state_dir: &Path) -> Result<()> {
    let pool = open(state_dir, true).await?;
    sqlx::migrate!("./migrations").run(&pool).await?;
    check_version(&pool).await?;
    pool.close().await;
    Ok(())
}

pub async fn check_version(pool: &SqlitePool) -> Result<()> {
    let row =
        sqlx::query("SELECT schema_version, app_version FROM app_metadata WHERE singleton = 1")
            .fetch_one(pool)
            .await
            .context("state is not initialized")?;
    if row.get::<i64, _>("schema_version") != SCHEMA_VERSION
        || row.get::<String, _>("app_version") != APP_VERSION
    {
        bail!("incompatible database/app version; stop services and run the matching release");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn initialization_is_repeatable_and_version_mismatches_fail_closed() {
        let dir = tempfile::tempdir().unwrap();
        initialize(dir.path()).await.unwrap();
        initialize(dir.path()).await.unwrap();
        let pool = connect(dir.path()).await.unwrap();
        check_version(&pool).await.unwrap();
        sqlx::query("UPDATE app_metadata SET schema_version = 999")
            .execute(&pool)
            .await
            .unwrap();
        assert!(check_version(&pool).await.is_err());
    }
}
