use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};
use sqlx::{Row, SqlitePool};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Seek, Write},
    os::{
        fd::AsRawFd,
        unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};
use wetube_backend::incoming::IncomingRoot;

pub struct ImportConfig {
    incoming: IncomingRoot,
    scratch: File,
    max_iso_bytes: u64,
    max_scratch_bytes: u64,
    staging_timeout: Duration,
}

impl ImportConfig {
    pub fn open(
        incoming: &Path,
        scratch: &Path,
        max_iso_bytes: u64,
        max_scratch_bytes: u64,
        staging_timeout: Duration,
    ) -> Result<Self> {
        ensure!(
            max_iso_bytes > 0 && max_scratch_bytes >= max_iso_bytes,
            "invalid staging byte limits"
        );
        ensure!(
            staging_timeout >= Duration::from_secs(1)
                && staging_timeout <= Duration::from_secs(3600),
            "staging timeout must be between 1 and 3600 seconds"
        );
        match std::fs::DirBuilder::new().mode(0o700).create(scratch) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error).context("create private scratch root"),
        }
        let scratch_file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(scratch)?;
        let metadata = scratch_file.metadata()?;
        ensure!(
            metadata.mode() & 0o077 == 0 && metadata.uid() == unsafe { libc::geteuid() },
            "scratch root must be owned by the worker UID and private (0700)"
        );
        let incoming_path = incoming.canonicalize()?;
        let scratch_path = scratch.canonicalize()?;
        ensure!(
            !incoming_path.starts_with(&scratch_path) && !scratch_path.starts_with(&incoming_path),
            "incoming and private scratch roots must not overlap"
        );
        Ok(Self {
            incoming: IncomingRoot::open(incoming)?,
            scratch: scratch_file,
            max_iso_bytes,
            max_scratch_bytes,
            staging_timeout,
        })
    }

    fn scratch_path(&self) -> PathBuf {
        PathBuf::from(format!("/proc/self/fd/{}", self.scratch.as_raw_fd()))
    }
}

fn usage(path: &Path, depth: usize, entries: &mut usize) -> Result<u64> {
    ensure!(
        depth <= 4,
        "unexpected scratch directory nesting; operator review required"
    );
    let mut total = 0u64;
    for entry in std::fs::read_dir(path)? {
        *entries += 1;
        ensure!(
            *entries <= 10_000,
            "too many scratch entries; operator review required"
        );
        let path = entry?.path();
        let metadata = std::fs::symlink_metadata(&path)?;
        let bytes = if metadata.is_dir() {
            usage(&path, depth + 1, entries)?
        } else {
            ensure!(
                metadata.is_file(),
                "unexpected scratch object; operator review required"
            );
            metadata.len()
        };
        total = total.checked_add(bytes).context("scratch size overflow")?;
    }
    Ok(total)
}

struct Staged {
    path: PathBuf,
    sha256: String,
    size_bytes: u64,
}

fn stage(config: &ImportConfig, name: &str, expected: &str, attempt: &str) -> Result<Staged> {
    let (mut source, entry) = config.incoming.open_file(name)?;
    ensure!(
        entry.identity == expected,
        "source changed since confirmation; refresh and reconfirm"
    );
    ensure!(
        entry.size_bytes <= config.max_iso_bytes,
        "ISO exceeds the configured per-file byte limit"
    );
    let scratch = config.scratch_path();
    let used = usage(&scratch, 0, &mut 0)?;
    ensure!(
        used.checked_add(entry.size_bytes)
            .is_some_and(|total| total <= config.max_scratch_bytes),
        "scratch quota would be exceeded; retained attempts need operator review"
    );
    ensure!(
        fs2::available_space(&scratch)?
            >= entry
                .size_bytes
                .checked_add(1024 * 1024 * 1024)
                .context("size overflow")?,
        "insufficient scratch space; reserve at least the ISO size plus 1 GiB"
    );
    let directory = scratch.join(attempt);
    std::fs::DirBuilder::new().mode(0o700).create(&directory)?;
    config.scratch.sync_all()?;
    let path = directory.join("input.iso");
    let mut target = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)?;
    let deadline = Instant::now() + config.staging_timeout;
    let mut buffer = vec![0u8; 1024 * 1024];
    let mut hash = Sha256::new();
    let mut count = 0u64;
    loop {
        ensure!(
            Instant::now() < deadline,
            "staging exceeded its configured copy/verification budget while copying"
        );
        let bytes = source.read(&mut buffer)?;
        if bytes == 0 {
            break;
        }
        count = count
            .checked_add(bytes as u64)
            .context("input size overflow")?;
        ensure!(
            count <= entry.size_bytes,
            "source grew while copying; input retained"
        );
        hash.update(&buffer[..bytes]);
        target.write_all(&buffer[..bytes])?;
    }
    ensure!(
        count == entry.size_bytes,
        "source was truncated while copying"
    );
    config.incoming.verify(name, &source, expected)?;
    target.sync_all()?;
    File::open(&directory)?.sync_all()?;
    let digest = hex::encode(hash.finalize());
    target.rewind()?;
    let mut verify = Sha256::new();
    loop {
        ensure!(
            Instant::now() < deadline,
            "staging exceeded its configured copy/verification budget while verifying"
        );
        let bytes = target.read(&mut buffer)?;
        if bytes == 0 {
            break;
        }
        verify.update(&buffer[..bytes]);
    }
    ensure!(
        hex::encode(verify.finalize()) == digest,
        "staged ISO checksum verification failed"
    );
    config.incoming.verify(name, &source, expected)?;
    Ok(Staged {
        path,
        sha256: digest,
        size_bytes: count,
    })
}

pub async fn recover(pool: &SqlitePool) -> Result<()> {
    sqlx::query(
        "UPDATE probe_jobs SET state = 'failed',
         error = 'Worker interrupted. Any attempt is retained; refresh and reconfirm the source for a new job.'
         WHERE state IN ('staging', 'probing')",
    ).execute(pool).await?;
    Ok(())
}

/// Hold worker ownership and drain this future on shutdown: blocking filesystem I/O cannot be cancelled.
pub async fn tick(pool: &SqlitePool, config: Arc<ImportConfig>) -> Result<bool> {
    let attempt = uuid::Uuid::new_v4().to_string();
    let row = sqlx::query(
        "UPDATE probe_jobs SET state = 'staging', attempt_id = ?
         WHERE id = (SELECT id FROM probe_jobs WHERE state = 'queued'
         ORDER BY created_at, rowid LIMIT 1) RETURNING id, name, source_identity, title, angle",
    )
    .bind(&attempt)
    .fetch_optional(pool)
    .await?;
    let Some(row) = row else {
        return Ok(false);
    };
    let id: String = row.get("id");
    let name: String = row.get("name");
    let expected: String = row.get("source_identity");
    let title = u8::try_from(row.get::<i64, _>("title"))?;
    let angle = u8::try_from(row.get::<i64, _>("angle"))?;
    let stage_attempt = attempt.clone();
    let stage_config = config.clone();
    let result: Result<serde_json::Value> = async {
        let staged = tokio::task::spawn_blocking(move || {
            stage(&stage_config, &name, &expected, &stage_attempt)
        })
        .await
        .context("staging task failed")??;
        let staged_report = serde_json::json!({
            "sha256": staged.sha256, "size_bytes": staged.size_bytes,
        });
        sqlx::query("UPDATE probe_jobs SET state = 'probing', report = ? WHERE id = ?")
            .bind(staged_report.to_string())
            .bind(&id)
            .execute(pool)
            .await?;
        let inspection = super::probe::inspect_dvd(&staged.path, title, angle).await?;
        Ok(serde_json::json!({
            "sha256": staged_report["sha256"],
            "size_bytes": staged.size_bytes,
            "inspection": inspection,
        }))
    }
    .await;
    match result {
        Ok(report) => {
            sqlx::query("UPDATE probe_jobs SET state = 'needs_review', report = ?, error = NULL WHERE id = ?")
                .bind(report.to_string()).bind(id).execute(pool).await?;
        }
        Err(error) => {
            let message: String = format!("{error:#}").chars().take(4096).collect();
            sqlx::query("UPDATE probe_jobs SET state = 'failed', error = ? WHERE id = ?")
                .bind(message)
                .bind(id)
                .execute(pool)
                .await?;
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stages_a_verified_copy_and_preserves_the_source() {
        let dir = tempfile::tempdir().unwrap();
        let incoming = dir.path().join("incoming");
        let scratch = dir.path().join("scratch");
        std::fs::create_dir(&incoming).unwrap();
        let source = incoming.join("movie.iso");
        std::fs::write(&source, b"private ISO copy").unwrap();
        let config =
            ImportConfig::open(&incoming, &scratch, 1024, 4096, Duration::from_secs(600)).unwrap();
        let expected = config.incoming.open_file("movie.iso").unwrap().1.identity;
        assert!(stage(&config, "movie.iso", "stale", "stale").is_err());
        let staged = stage(&config, "movie.iso", &expected, "attempt").unwrap();
        assert_eq!(std::fs::read(&staged.path).unwrap(), b"private ISO copy");
        assert_eq!(
            staged.sha256,
            hex::encode(Sha256::digest(b"private ISO copy"))
        );
        assert_eq!(std::fs::read(source).unwrap(), b"private ISO copy");
        assert!(stage(&config, "movie.iso", &expected, "attempt").is_err());
    }

    #[test]
    fn rejects_overlapping_roots_and_quota_exhaustion() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            ImportConfig::open(dir.path(), dir.path(), 1, 1, Duration::from_secs(600)).is_err()
        );
        let incoming = dir.path().join("incoming");
        let scratch = dir.path().join("scratch");
        std::fs::create_dir(&incoming).unwrap();
        std::fs::write(incoming.join("movie.iso"), b"too large").unwrap();
        let config =
            ImportConfig::open(&incoming, &scratch, 4, 4, Duration::from_secs(600)).unwrap();
        let expected = config.incoming.open_file("movie.iso").unwrap().1.identity;
        assert!(stage(&config, "movie.iso", &expected, "attempt").is_err());
        assert_eq!(usage(&config.scratch_path(), 0, &mut 0).unwrap(), 0);
        let config =
            ImportConfig::open(&incoming, &scratch, 10, 10, Duration::from_secs(600)).unwrap();
        std::fs::write(scratch.join("retained-partial.iso"), b"retained").unwrap();
        let error = stage(&config, "movie.iso", &expected, "attempt")
            .err()
            .unwrap();
        assert!(error.to_string().contains("scratch quota"));
        assert!(!scratch.join("attempt").exists());
        std::os::unix::fs::symlink(incoming.join("movie.iso"), scratch.join("unsafe.iso")).unwrap();
        assert!(usage(&config.scratch_path(), 0, &mut 0).is_err());
    }

    #[test]
    fn refuses_shared_or_symlink_scratch_roots() {
        use std::os::unix::fs::PermissionsExt;
        let incoming = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let scratch = dir.path().join("scratch");
        std::fs::create_dir(&scratch).unwrap();
        std::fs::set_permissions(&scratch, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            ImportConfig::open(
                incoming.path(),
                &scratch,
                100,
                100,
                Duration::from_secs(600)
            )
            .is_err()
        );
        std::fs::set_permissions(&scratch, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::os::unix::fs::symlink(&scratch, dir.path().join("link")).unwrap();
        assert!(
            ImportConfig::open(
                incoming.path(),
                &dir.path().join("link"),
                100,
                100,
                Duration::from_secs(600)
            )
            .is_err()
        );
    }

    #[test]
    fn refuses_unbounded_staging_budgets_before_creating_scratch() {
        let incoming = tempfile::tempdir().unwrap();
        let scratch = incoming.path().join("not-created");
        for timeout in [Duration::ZERO, Duration::from_secs(3601)] {
            assert!(ImportConfig::open(incoming.path(), &scratch, 100, 100, timeout).is_err());
        }
        assert!(!scratch.exists());
    }

    #[tokio::test]
    async fn failures_and_recovery_retain_input_and_never_automatically_retry() {
        use wetube_backend::{db, probe_jobs};
        use wetube_core::{ProbeRequest, ProbeState};
        let dir = tempfile::tempdir().unwrap();
        let incoming = tempfile::tempdir().unwrap();
        let scratch = dir.path().join("scratch");
        std::fs::write(incoming.path().join("movie.iso"), b"not a DVD").unwrap();
        let config = Arc::new(
            ImportConfig::open(
                incoming.path(),
                &scratch,
                1024,
                4096,
                Duration::from_secs(600),
            )
            .unwrap(),
        );
        db::initialize(dir.path()).await.unwrap();
        let pool = db::connect(dir.path()).await.unwrap();
        sqlx::query("INSERT INTO users VALUES ('admin', 'admin', 'test hash', 'admin')")
            .execute(&pool)
            .await
            .unwrap();
        let request = ProbeRequest {
            name: "movie.iso".into(),
            identity: config.incoming.open_file("movie.iso").unwrap().1.identity,
            title: 1,
            angle: 1,
            copy_complete: true,
        };
        probe_jobs::enqueue(&pool, "admin", &request)
            .await
            .unwrap()
            .unwrap();
        assert!(tick(&pool, config.clone()).await.unwrap());
        let failed = probe_jobs::list(&pool).await.unwrap().remove(0);
        assert_eq!(failed.state, ProbeState::Failed);
        let error = failed.error.unwrap();
        assert!(error.contains("DVD inspection"), "{error}");
        assert!(failed.report.as_ref().unwrap()["sha256"].is_string());
        let retained = scratch.join(failed.attempt_id.unwrap()).join("input.iso");
        assert_eq!(std::fs::read(&retained).unwrap(), b"not a DVD");
        assert_eq!(
            std::fs::read(incoming.path().join("movie.iso")).unwrap(),
            b"not a DVD"
        );

        let second = probe_jobs::enqueue(&pool, "admin", &request)
            .await
            .unwrap()
            .unwrap();
        std::fs::write(incoming.path().join("movie.iso"), b"changed copy").unwrap();
        tick(&pool, config.clone()).await.unwrap();
        let second = probe_jobs::list(&pool)
            .await
            .unwrap()
            .into_iter()
            .find(|job| job.id == second.id)
            .unwrap();
        assert_eq!(second.state, ProbeState::Failed);
        assert!(second.error.unwrap().contains("source changed"));
        assert!(!scratch.join(second.attempt_id.unwrap()).exists());

        sqlx::query("UPDATE probe_jobs SET state = 'probing' WHERE id = ?")
            .bind(&failed.id)
            .execute(&pool)
            .await
            .unwrap();
        recover(&pool).await.unwrap();
        let recovered = probe_jobs::list(&pool)
            .await
            .unwrap()
            .into_iter()
            .find(|job| job.id == failed.id)
            .unwrap();
        assert_eq!(recovered.state, ProbeState::Failed);
        assert!(recovered.error.unwrap().contains("interrupted"));
        assert!(retained.is_file());
        assert!(!tick(&pool, config).await.unwrap());
        pool.close().await;
    }
}
