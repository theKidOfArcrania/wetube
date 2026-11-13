use anyhow::{Context, Result, bail};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{Row, SqlitePool};
use std::os::unix::fs::DirBuilderExt;
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
    let has_metadata: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'app_metadata'",
    )
    .fetch_one(&pool)
    .await?;
    if has_metadata != 0 {
        let version: (i64, String) = sqlx::query_as(
            "SELECT schema_version, app_version FROM app_metadata WHERE singleton = 1",
        )
        .fetch_one(&pool)
        .await?;
        anyhow::ensure!(
            (1..=SCHEMA_VERSION).contains(&version.0) && version.1 == APP_VERSION,
            "incompatible state; migration is not authorized for this release"
        );
        if version.0 < SCHEMA_VERSION {
            // This upgrade is from the foundation schema, before any subtitle assets exist.
            anyhow::ensure!(
                version.0 == 1,
                "unsupported upgrade; requires a complete asset-aware backup"
            );
            let backup = state_dir.join(format!("pre-upgrade-{}", uuid::Uuid::new_v4()));
            std::fs::DirBuilder::new().mode(0o700).create(&backup)?;
            let database = backup.join("wetube.sqlite3");
            sqlx::query("VACUUM INTO ?")
                .bind(
                    database
                        .to_str()
                        .context("state path must be UTF-8 for backup")?,
                )
                .execute(&pool)
                .await
                .context("create consistent pre-upgrade snapshot")?;
            std::fs::File::open(&database)?.sync_all()?;
            std::fs::File::open(&backup)?.sync_all()?;
            std::fs::File::open(state_dir)?.sync_all()?;
        }
    }
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
        assert!(initialize(dir.path()).await.is_err());
    }

    #[tokio::test]
    async fn upgrades_foundation_only_after_a_consistent_private_backup() {
        let dir = tempfile::tempdir().unwrap();
        let migrations = tempfile::tempdir().unwrap();
        std::fs::write(
            migrations.path().join("0001_foundation.sql"),
            include_str!("../migrations/0001_foundation.sql"),
        )
        .unwrap();
        let pool = open(dir.path(), true).await.unwrap();
        sqlx::migrate::Migrator::new(migrations.path())
            .await
            .unwrap()
            .run(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO users VALUES ('retained', 'admin', 'test hash', 'admin')")
            .execute(&pool)
            .await
            .unwrap();
        pool.close().await;
        initialize(dir.path()).await.unwrap();
        initialize(dir.path()).await.unwrap();
        let backups: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("pre-upgrade-")
            })
            .collect();
        assert_eq!(backups.len(), 1);
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&backups[0]).unwrap().permissions().mode() & 0o077,
            0
        );
        let backup = SqlitePoolOptions::new()
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(backups[0].join("wetube.sqlite3"))
                    .read_only(true),
            )
            .await
            .unwrap();
        let old_version: i64 = sqlx::query_scalar("SELECT schema_version FROM app_metadata")
            .fetch_one(&backup)
            .await
            .unwrap();
        assert_eq!(old_version, 1);
        let integrity: String = sqlx::query_scalar("PRAGMA integrity_check")
            .fetch_one(&backup)
            .await
            .unwrap();
        assert_eq!(integrity, "ok");
        for database in [&backup, &connect(dir.path()).await.unwrap()] {
            let id: String = sqlx::query_scalar("SELECT id FROM users")
                .fetch_one(database)
                .await
                .unwrap();
            assert_eq!(id, "retained");
        }
        backup.close().await;
    }
}
