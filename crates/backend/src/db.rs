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
            // This additive migration does not modify registered media. Snapshot
            // SQLite first; operators must retain the separate media/attempt roots.
            anyhow::ensure!(
                matches!(version.0, 1..=10),
                "unsupported upgrade; requires a complete asset-aware backup"
            );
            let backup = state_dir.join(format!("pre-upgrade-{}", uuid::Uuid::new_v4()));
            eprintln!(
                "SQLite-only snapshot: {}. Registered media and attempts are not copied; retain them in place.",
                backup.display()
            );
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
    async fn posters_upgrade_from_ten_preserves_movies_labels_and_snapshots_once() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let pool = open(directory.path(), true).await?;
        let mut migrations = sqlx::migrate!("./migrations");
        migrations.migrations = std::borrow::Cow::Owned(
            migrations
                .iter()
                .filter(|migration| migration.version <= 10)
                .cloned()
                .collect(),
        );
        migrations.run(&pool).await?;
        sqlx::query("INSERT INTO movies VALUES('movie','Local',2000,'Original','ready')")
            .execute(&pool)
            .await?;
        sqlx::query("INSERT INTO tmdb_metadata VALUES('movie',7,'Provider',2001,'Synopsis',1,2)")
            .execute(&pool)
            .await?;
        let labels = wetube_core::MovieLabels {
            categories: vec!["Family".into()],
            tags: vec!["Favorites".into()],
        };
        assert_eq!(
            crate::labels::update(
                &pool,
                "movie",
                &wetube_core::UpdateMovieLabels {
                    expected: wetube_core::MovieLabels::default(),
                    labels: labels.clone(),
                }
            )
            .await?,
            crate::labels::UpdateResult::Saved(labels.clone())
        );
        pool.close().await;
        initialize(directory.path()).await?;
        initialize(directory.path()).await?;
        let pool = connect(directory.path()).await?;
        check_version(&pool).await?;
        assert_eq!(
            sqlx::query_scalar::<_, String>("SELECT title FROM tmdb_metadata")
                .fetch_one(&pool)
                .await?,
            "Provider"
        );
        assert_eq!(crate::labels::for_movie(&pool, "movie").await?, labels);
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM tmdb_posters")
                .fetch_one(&pool)
                .await?,
            0
        );
        assert_eq!(
            std::fs::read_dir(directory.path())?
                .filter_map(Result::ok)
                .filter(|entry| entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("pre-upgrade-"))
                .count(),
            1
        );
        pool.close().await;
        Ok(())
    }

    #[tokio::test]
    async fn labels_upgrade_from_nine_preserves_provider_overlay_and_snapshots_once() -> Result<()>
    {
        let directory = tempfile::tempdir()?;
        let pool = open(directory.path(), true).await?;
        let mut migrations = sqlx::migrate!("./migrations");
        migrations.migrations = std::borrow::Cow::Owned(
            migrations
                .iter()
                .filter(|migration| migration.version <= 9)
                .cloned()
                .collect(),
        );
        migrations.run(&pool).await?;
        sqlx::query("INSERT INTO movies VALUES('movie','Local',2000,'Original','ready')")
            .execute(&pool)
            .await?;
        sqlx::query("INSERT INTO tmdb_metadata VALUES('movie',7,'Provider',2001,'Synopsis',1,2)")
            .execute(&pool)
            .await?;
        pool.close().await;
        initialize(directory.path()).await?;
        initialize(directory.path()).await?;
        let pool = connect(directory.path()).await?;
        assert_eq!(
            sqlx::query_scalar::<_, String>("SELECT title FROM tmdb_metadata")
                .fetch_one(&pool)
                .await?,
            "Provider"
        );
        assert_eq!(
            crate::labels::for_movie(&pool, "movie").await?,
            wetube_core::MovieLabels::default()
        );
        let snapshots = std::fs::read_dir(directory.path())?
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("pre-upgrade-")
            })
            .count();
        assert_eq!(snapshots, 1);
        pool.close().await;
        Ok(())
    }

    #[tokio::test]
    async fn metadata_upgrade_from_eight_preserves_local_movies_and_snapshots_once() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let pool = open(directory.path(), true).await?;
        let mut migrations = sqlx::migrate!("./migrations");
        migrations.migrations = std::borrow::Cow::Owned(
            migrations
                .iter()
                .filter(|migration| migration.version <= 8)
                .cloned()
                .collect(),
        );
        migrations.run(&pool).await?;
        sqlx::query(
            "INSERT INTO movies VALUES ('movie','Manual title',1999,'Manual description','ready')",
        )
        .execute(&pool)
        .await?;
        pool.close().await;
        initialize(directory.path()).await?;
        initialize(directory.path()).await?;
        let pool = connect(directory.path()).await?;
        let row = sqlx::query("SELECT * FROM movies WHERE id='movie'")
            .fetch_one(&pool)
            .await?;
        assert_eq!(row.get::<String, _>("title"), "Manual title");
        assert_eq!(row.get::<i32, _>("year"), 1999);
        assert_eq!(row.get::<String, _>("description"), "Manual description");
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM tmdb_metadata")
                .fetch_one(&pool)
                .await?,
            0
        );
        assert_eq!(
            std::fs::read_dir(directory.path())?
                .filter_map(Result::ok)
                .filter(|entry| entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("pre-upgrade-"))
                .count(),
            1
        );
        pool.close().await;
        Ok(())
    }

    #[tokio::test]
    async fn comparison_progress_upgrade_retains_schema_seven_jobs_reviews_and_personal_history()
    -> Result<()> {
        let directory = tempfile::tempdir()?;
        let migrations = tempfile::tempdir()?;
        for (name, sql) in [
            (
                "0001_foundation.sql",
                include_str!("../migrations/0001_foundation.sql"),
            ),
            (
                "0002_probe_jobs.sql",
                include_str!("../migrations/0002_probe_jobs.sql"),
            ),
            (
                "0003_local_preview.sql",
                include_str!("../migrations/0003_local_preview.sql"),
            ),
            (
                "0004_conversions.sql",
                include_str!("../migrations/0004_conversions.sql"),
            ),
            (
                "0005_media_progress.sql",
                include_str!("../migrations/0005_media_progress.sql"),
            ),
            (
                "0006_import_history.sql",
                include_str!("../migrations/0006_import_history.sql"),
            ),
            (
                "0007_comparisons.sql",
                include_str!("../migrations/0007_comparisons.sql"),
            ),
        ] {
            std::fs::write(migrations.path().join(name), sql)?;
        }
        let pool = open(directory.path(), true).await?;
        sqlx::migrate::Migrator::new(migrations.path())
            .await?
            .run(&pool)
            .await?;
        sqlx::query("INSERT INTO users VALUES ('admin','admin','hash','admin')")
            .execute(&pool)
            .await?;
        sqlx::query("INSERT INTO scan_jobs(id,requested_by,name,source_identity,state,created_at,attempt_id,report)
            VALUES ('scan','admin','count.iso','bound','needs_review',1,'retained-scan','{\"titles\":[]}')").execute(&pool).await?;
        sqlx::query("INSERT INTO probe_jobs(id,requested_by,name,source_identity,title,angle,state,created_at,scan_job_id,attempt_id,report)
            VALUES ('probe','admin','count.iso','bound',1,1,'needs_review',1,'scan','retained-scan','{\"sha256\":\"retained\"}')").execute(&pool).await?;
        sqlx::query("INSERT INTO comparison_jobs(id,probe_job_id,requested_by,state,created_at,probe_report,title,angle,attempt_id,report)
            SELECT 'comparison',id,'admin','succeeded',1,report,title,angle,'retained-comparison','{\"status\":\"succeeded\"}'
            FROM probe_jobs WHERE id='probe'").execute(&pool).await?;
        sqlx::query(
            "INSERT INTO import_preferences(user_id,kind,job_id,archived)
            VALUES ('admin','scan','scan',1)",
        )
        .execute(&pool)
        .await?;
        pool.close().await;
        initialize(directory.path()).await?;
        initialize(directory.path()).await?;
        let pool = connect(directory.path()).await?;
        let probe = crate::probe_jobs::list(&pool).await?.remove(0);
        assert_eq!(probe.state, wetube_core::ProbeState::NeedsReview);
        assert_eq!(probe.attempt_id.as_deref(), Some("retained-scan"));
        assert_eq!(probe.report, Some(serde_json::json!({"sha256":"retained"})));
        assert_eq!(crate::scans::list(&pool).await?.len(), 1);
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT archived FROM import_preferences WHERE user_id='admin'"
            )
            .fetch_one(&pool)
            .await?,
            1
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM comparison_jobs")
                .fetch_one(&pool)
                .await?,
            1
        );
        let comparison = crate::comparisons::latest(&pool, "probe").await?.unwrap();
        assert_eq!(comparison.state, wetube_core::JobState::Succeeded);
        assert_eq!(
            comparison.attempt_id.as_deref(),
            Some("retained-comparison")
        );
        assert!(comparison.progress.is_none());
        assert_eq!(
            comparison.report,
            Some(serde_json::json!({"status":"succeeded"}))
        );
        pool.close().await;
        Ok(())
    }

    #[tokio::test]
    async fn history_upgrade_preserves_schema_five_jobs_and_snapshots_once() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let migrations = tempfile::tempdir()?;
        for (name, sql) in [
            (
                "0001_foundation.sql",
                include_str!("../migrations/0001_foundation.sql"),
            ),
            (
                "0002_probe_jobs.sql",
                include_str!("../migrations/0002_probe_jobs.sql"),
            ),
            (
                "0003_local_preview.sql",
                include_str!("../migrations/0003_local_preview.sql"),
            ),
            (
                "0004_conversions.sql",
                include_str!("../migrations/0004_conversions.sql"),
            ),
            (
                "0005_media_progress.sql",
                include_str!("../migrations/0005_media_progress.sql"),
            ),
        ] {
            std::fs::write(migrations.path().join(name), sql)?;
        }
        let pool = open(directory.path(), true).await?;
        sqlx::migrate::Migrator::new(migrations.path())
            .await?
            .run(&pool)
            .await?;
        sqlx::query("INSERT INTO users VALUES ('admin','admin','hash','admin')")
            .execute(&pool)
            .await?;
        sqlx::query("INSERT INTO scan_jobs(id,requested_by,name,source_identity,state,created_at,attempt_id,report) VALUES ('scan','admin','test.iso','bound','needs_review',1,'retained-scan','{\"titles\":[]}')").execute(&pool).await?;
        sqlx::query("INSERT INTO probe_jobs(id,requested_by,name,source_identity,title,angle,state,created_at,scan_job_id,attempt_id,report) VALUES ('probe','admin','test.iso','bound',1,1,'needs_review',1,'scan','retained-probe','{}')").execute(&pool).await?;
        sqlx::query("INSERT INTO conversion_jobs(id,requested_by,probe_job_id,state,created_at,video_treatment,attempt_id,report) VALUES ('conversion','admin','probe','failed',1,'preserve','retained-conversion','{}')").execute(&pool).await?;
        pool.close().await;
        initialize(directory.path()).await?;
        initialize(directory.path()).await?;
        let pool = connect(directory.path()).await?;
        let scan = crate::scans::list(&pool).await?.remove(0);
        assert_eq!(scan.attempt_id.as_deref(), Some("retained-scan"));
        assert!(scan.progress.is_none());
        let probe = crate::probe_jobs::list(&pool).await?.remove(0);
        assert_eq!(probe.attempt_id.as_deref(), Some("retained-probe"));
        assert!(probe.progress.is_none());
        let conversion = crate::conversions::list(&pool).await?.remove(0);
        assert_eq!(
            conversion.attempt_id.as_deref(),
            Some("retained-conversion")
        );
        assert!(conversion.progress.is_none());
        assert_eq!(conversion.state, wetube_core::ConversionState::Failed);
        let history = crate::import_history::page(&pool, "admin", true, None, 10).await?;
        assert_eq!(
            history.total, 1,
            "only the consumed scan collapses; failed conversion returns its probe to attention"
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM import_preferences")
                .fetch_one(&pool)
                .await?,
            0,
            "migration does not acknowledge anyone's failures"
        );
        let backups: Vec<_> = std::fs::read_dir(directory.path())?
            .filter_map(|entry| entry.ok())
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("pre-upgrade-")
            })
            .collect();
        assert_eq!(backups.len(), 1);
        let snapshot = SqlitePoolOptions::new()
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(backups[0].path().join("wetube.sqlite3"))
                    .read_only(true),
            )
            .await?;
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT schema_version FROM app_metadata")
                .fetch_one(&snapshot)
                .await?,
            5
        );
        assert!(
            sqlx::query("PRAGMA foreign_key_check")
                .fetch_all(&pool)
                .await?
                .is_empty()
        );
        snapshot.close().await;
        pool.close().await;
        Ok(())
    }

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
        let dir = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        let migrations = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
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

    #[tokio::test]
    async fn preview_upgrade_snapshots_schema_two_and_preserves_ready_catalogue() {
        let dir = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        let migrations = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        std::fs::write(
            migrations.path().join("0001_foundation.sql"),
            include_str!("../migrations/0001_foundation.sql"),
        )
        .unwrap();
        std::fs::write(
            migrations.path().join("0002_probe_jobs.sql"),
            include_str!("../migrations/0002_probe_jobs.sql"),
        )
        .unwrap();
        let pool = open(dir.path(), true).await.unwrap();
        sqlx::migrate::Migrator::new(migrations.path())
            .await
            .unwrap()
            .run(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO movies (id,title,status) VALUES ('seed','Retained seed','ready')")
            .execute(&pool)
            .await
            .unwrap();
        pool.close().await;
        initialize(dir.path()).await.unwrap();
        let pool = connect(dir.path()).await.unwrap();
        let title: String = sqlx::query_scalar("SELECT title FROM movies WHERE status='ready'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(title, "Retained seed");
        let backup = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("pre-upgrade-")
            })
            .unwrap();
        let snapshot = SqlitePoolOptions::new()
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(backup.join("wetube.sqlite3"))
                    .read_only(true),
            )
            .await
            .unwrap();
        let version: i64 = sqlx::query_scalar("SELECT schema_version FROM app_metadata")
            .fetch_one(&snapshot)
            .await
            .unwrap();
        assert_eq!(version, 2);
        let preview_tables: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM sqlite_master WHERE name='preview_media'")
                .fetch_one(&snapshot)
                .await
                .unwrap();
        assert_eq!(preview_tables, 0);
    }

    #[tokio::test]
    async fn conversion_upgrade_preserves_schema_three_media_and_staged_probe_registry() {
        let dir = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        let migrations = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        for (name, sql) in [
            (
                "0001_foundation.sql",
                include_str!("../migrations/0001_foundation.sql"),
            ),
            (
                "0002_probe_jobs.sql",
                include_str!("../migrations/0002_probe_jobs.sql"),
            ),
            (
                "0003_local_preview.sql",
                include_str!("../migrations/0003_local_preview.sql"),
            ),
        ] {
            std::fs::write(migrations.path().join(name), sql).unwrap();
        }
        let pool = open(dir.path(), true).await.unwrap();
        sqlx::migrate::Migrator::new(migrations.path())
            .await
            .unwrap()
            .run(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO users VALUES ('admin','admin','hash','admin')")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO movies(id,title,status) VALUES ('movie','Retained preview','ready')",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO preview_media VALUES ('movie','retained-package','retained-hash',100,1.0,'retained-identity',1)")
            .execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO probe_jobs(id,requested_by,name,source_identity,title,angle,state,created_at,attempt_id,report) VALUES ('probe','admin','source.iso','identity',1,1,'needs_review',1,'retained-attempt','{}')")
            .execute(&pool).await.unwrap();
        pool.close().await;
        initialize(dir.path()).await.unwrap();
        let pool = connect(dir.path()).await.unwrap();
        let media: (String, String, String) = sqlx::query_as(
            "SELECT package_id,sha256,file_identity FROM preview_media WHERE movie_id='movie'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            media,
            (
                "retained-package".into(),
                "retained-hash".into(),
                "retained-identity".into()
            )
        );
        let attempt: String =
            sqlx::query_scalar("SELECT attempt_id FROM probe_jobs WHERE id='probe'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(attempt, "retained-attempt");
        let ready: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM movies WHERE id='movie' AND status='ready'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(ready, 1);
        let foreign_keys = sqlx::query("PRAGMA foreign_key_check")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert!(foreign_keys.is_empty());
        let backups = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("pre-upgrade-")
            })
            .count();
        assert_eq!(
            backups, 1,
            "SQLite-only snapshot is created before the additive registry upgrade"
        );
        pool.close().await;
    }
}
