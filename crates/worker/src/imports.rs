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
            incoming.is_absolute() && scratch.is_absolute(),
            "incoming and scratch roots must be explicit absolute paths"
        );
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
            Err(error) => return Err(error).context("create scratch root"),
        }
        let scratch_file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(scratch)?;
        let incoming_path = incoming.canonicalize()?;
        let scratch_path = scratch.canonicalize()?;
        ensure!(
            !incoming_path.starts_with(&scratch_path) && !scratch_path.starts_with(&incoming_path),
            "incoming and scratch roots must not overlap"
        );
        Ok(Self {
            incoming: IncomingRoot::open(incoming)?,
            scratch: scratch_file,
            max_iso_bytes,
            max_scratch_bytes,
            staging_timeout,
        })
    }

    pub(crate) fn scratch_path(&self) -> PathBuf {
        PathBuf::from(format!("/proc/self/fd/{}", self.scratch.as_raw_fd()))
    }

    pub fn output_root(&self) -> Result<wetube_backend::preview::PreviewRoot> {
        wetube_backend::preview::PreviewRoot::from_directory(self.scratch.try_clone()?)
    }

    pub fn acquire_scratch_lock(&self) -> Result<wetube_backend::lock::MaintenanceLock> {
        wetube_backend::lock::MaintenanceLock::acquire(&self.scratch_path())
    }

    pub fn require_separate(&self, other: &Path) -> Result<()> {
        let scratch = self.scratch_path().canonicalize()?;
        let other = other.canonicalize()?;
        ensure!(
            !scratch.starts_with(&other) && !other.starts_with(&scratch),
            "scratch and state roots must not overlap"
        );
        Ok(())
    }

    pub(crate) fn require_capacity(&self, bytes: u64) -> Result<()> {
        let scratch = self.scratch_path();
        let used = usage(&scratch, 0, &mut 0)?;
        ensure!(
            used.checked_add(bytes)
                .is_some_and(|total| total <= self.max_scratch_bytes),
            "scratch quota would be exceeded; retained attempts need operator review"
        );
        ensure!(
            fs2::available_space(&scratch)?
                >= bytes
                    .checked_add(1024 * 1024 * 1024)
                    .context("size overflow")?,
            "insufficient scratch space; reserve at least the requested bytes plus 1 GiB"
        );
        Ok(())
    }

    pub(crate) fn source_size(&self, name: &str, expected: &str) -> Result<u64> {
        let (_, entry) = self.incoming.open_file(name)?;
        ensure!(
            entry.size_bytes <= self.max_iso_bytes,
            "ISO exceeds the configured per-file byte limit"
        );
        ensure!(
            entry.identity == expected,
            "source changed since confirmation; refresh and reconfirm"
        );
        Ok(entry.size_bytes)
    }

    pub(crate) fn storage(&self) -> Result<(u64, u64, u64, u64)> {
        Ok((
            self.scratch.metadata()?.dev(),
            fs2::available_space(self.scratch_path())?,
            usage(&self.scratch_path(), 0, &mut 0)?,
            self.max_scratch_bytes,
        ))
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

pub(crate) struct Staged {
    pub path: PathBuf,
    pub file: File,
    pub sha256: String,
    pub size_bytes: u64,
    pub identity: String,
    pub(crate) _directory: Option<File>,
}

impl Staged {
    pub(crate) fn verify(&self) -> Result<()> {
        ensure!(
            wetube_backend::incoming::identity(&self.file.metadata()?) == self.identity
                && wetube_backend::incoming::identity(&std::fs::symlink_metadata(&self.path)?)
                    == self.identity,
            "verified staged input changed or was replaced"
        );
        Ok(())
    }
}

pub(crate) fn stage(
    config: &ImportConfig,
    name: &str,
    expected: &str,
    attempt: &str,
) -> Result<Staged> {
    stage_progress(config, name, expected, attempt, None)
}

pub(crate) fn stage_progress(
    config: &ImportConfig,
    name: &str,
    expected: &str,
    attempt: &str,
    progress: Option<&super::progress::Progress>,
) -> Result<Staged> {
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
    config.require_capacity(entry.size_bytes)?;
    let directory = scratch.join(attempt);
    std::fs::DirBuilder::new().mode(0o700).create(&directory)?;
    config.scratch.sync_all()?;
    let path = directory.join("input.iso");
    let mut target = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)?;
    let deadline = Instant::now() + config.staging_timeout;
    if let Some(progress) = progress {
        progress.stage(
            0,
            "Copying private input",
            Some(entry.size_bytes as f64),
            "bytes",
        );
    }
    let mut buffer = vec![0u8; 1024 * 1024];
    let mut hash = Sha256::new();
    let mut count = 0u64;
    loop {
        if let Some(progress) = progress {
            progress.check()?;
        }
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
        if let Some(progress) = progress {
            progress.units(count as f64);
        }
    }
    ensure!(
        count == entry.size_bytes,
        "source was truncated while copying"
    );
    config.incoming.verify(name, &source, expected)?;
    target.sync_all()?;
    File::open(&directory)?.sync_all()?;
    // CIFS may reconcile write timestamps at close; fingerprint only the final read handle.
    drop(target);
    let mut target = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&path)?;
    let digest = hex::encode(hash.finalize());
    let staged_identity = wetube_backend::incoming::identity(&target.metadata()?);
    let mut verify = Sha256::new();
    let mut verified = 0u64;
    if let Some(progress) = progress {
        progress.stage(
            1,
            "Verifying private input checksum",
            Some(count as f64),
            "bytes",
        );
    }
    loop {
        if let Some(progress) = progress {
            progress.check()?;
        }
        ensure!(
            Instant::now() < deadline,
            "staging exceeded its configured copy/verification budget while verifying"
        );
        let bytes = target.read(&mut buffer)?;
        if bytes == 0 {
            break;
        }
        verify.update(&buffer[..bytes]);
        verified += bytes as u64;
        if let Some(progress) = progress {
            progress.units(verified as f64);
        }
    }
    ensure!(
        hex::encode(verify.finalize()) == digest,
        "staged ISO checksum verification failed"
    );
    ensure!(
        wetube_backend::incoming::identity(&target.metadata()?) == staged_identity
            && wetube_backend::incoming::identity(&std::fs::symlink_metadata(&path)?)
                == staged_identity,
        "staged ISO changed during checksum verification"
    );
    config.incoming.verify(name, &source, expected)?;
    target.rewind()?;
    Ok(Staged {
        path,
        file: target,
        sha256: digest,
        size_bytes: count,
        identity: staged_identity,
        _directory: None,
    })
}

#[cfg(test)]
pub(crate) fn revalidate(config: &ImportConfig, probe: &wetube_core::ProbeJob) -> Result<Staged> {
    revalidate_progress(config, probe, None)
}

pub(crate) fn revalidate_progress(
    config: &ImportConfig,
    probe: &wetube_core::ProbeJob,
    progress: Option<&super::progress::Progress>,
) -> Result<Staged> {
    let report = probe.report.as_ref().context("probe has no report")?;
    let attempt = uuid::Uuid::parse_str(
        report["staged_attempt_id"]
            .as_str()
            .or(probe.attempt_id.as_deref())
            .context("probe has no staged attempt")?,
    )?;
    let expected = report["sha256"]
        .as_str()
        .context("probe has no staged checksum")?;
    let size = report["size_bytes"]
        .as_u64()
        .context("probe has no staged size")?;
    ensure!(
        size > 0 && size <= config.max_iso_bytes,
        "staged size exceeds input quota"
    );
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(config.scratch_path().join(attempt.to_string()))?;
    let path = PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd())).join("input.iso");
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&path)?;
    ensure!(
        file.metadata()?.is_file() && file.metadata()?.len() == size,
        "staged input size changed"
    );
    let before = wetube_backend::incoming::identity(&file.metadata()?);
    if let Some(progress) = progress {
        progress.stage(
            0,
            "Revalidating retained input checksum",
            Some(size as f64),
            "bytes",
        );
    }
    let sha256 = super::experimental::bounded_hash_progress(
        &mut file,
        size,
        config.staging_timeout,
        progress,
    )?;
    ensure!(
        sha256 == expected,
        "staged input checksum mismatch; prior outputs cannot be reused"
    );
    let staged = Staged {
        path,
        file,
        sha256,
        size_bytes: size,
        identity: before,
        _directory: Some(directory),
    };
    staged.verify()?;
    Ok(staged)
}

pub async fn recover(pool: &SqlitePool) -> Result<()> {
    sqlx::query(
        "UPDATE probe_jobs SET state = 'failed',
         error = 'Worker interrupted. Any attempt is retained; refresh and reconfirm the source for a new job.'
         WHERE state IN ('staging', 'probing')
         AND NOT EXISTS(SELECT 1 FROM processing_queue WHERE job_id=probe_jobs.id AND kind='probe' AND state IN ('running','cancelling') AND finish_state IS NOT NULL)",
    ).execute(pool).await?;
    Ok(())
}

/// Drain this future on shutdown and cancellation; blocking I/O cooperates between chunks.
pub async fn tick(pool: &SqlitePool, config: Arc<ImportConfig>) -> Result<bool> {
    let Some(claim) = wetube_backend::queue::next(pool, "probe").await? else {
        return Ok(false);
    };
    let key = claim.key;
    let result = run(pool, config, claim.row).await;
    wetube_backend::queue::finish(pool, key).await?;
    result
}

pub(crate) async fn run(
    pool: &SqlitePool,
    config: Arc<ImportConfig>,
    row: sqlx::sqlite::SqliteRow,
) -> Result<bool> {
    let attempt: String = row.try_get("attempt_id")?;
    let id: String = row.get("id");
    let name: String = row.get("name");
    let expected: String = row.get("source_identity");
    let title = u8::try_from(row.get::<i64, _>("title"))?;
    let angle = u8::try_from(row.get::<i64, _>("angle"))?;
    let stage_attempt = attempt.clone();
    let stage_config = config.clone();
    let staged_probe = wetube_backend::probe_jobs::from_row(&row)?;
    let reuse = row.try_get::<Option<String>, _>("scan_job_id")?.is_some();
    let recording = super::progress::Recording::start(
        pool,
        "probe_jobs",
        &id,
        if reuse {
            wetube_core::MediaWorkflow::Reinspect
        } else {
            wetube_core::MediaWorkflow::Inspect
        },
        0,
    );
    let progress = &recording.progress;
    let staging_progress = progress.clone();
    let result: Result<serde_json::Value> = async {
        if let Some(scan) = row.try_get::<Option<String>, _>("scan_job_id")? {
            wetube_backend::simple_imports::validate_scan_authorization(pool, &scan).await?;
        }
        let staged = tokio::task::spawn_blocking(move || {
            if reuse { revalidate_progress(&stage_config,&staged_probe,Some(&staging_progress)) } else { stage_progress(&stage_config, &name, &expected, &stage_attempt,Some(&staging_progress)) }
        })
        .await
        .context("staging task failed")??;
        let staged_report = serde_json::json!({
            "sha256": staged.sha256, "size_bytes": staged.size_bytes,
            "staged_attempt_id": if reuse {
                row.try_get::<Option<String>,_>("report")?.map(|report|serde_json::from_str::<serde_json::Value>(&report)).transpose()?
                    .and_then(|report|report["staged_attempt_id"].as_str().map(str::to_owned))
            } else { Some(attempt.clone()) },
        });
        sqlx::query("UPDATE probe_jobs SET state = 'probing', report = ? WHERE id = ?")
            .bind(staged_report.to_string())
            .bind(&id)
            .execute(pool)
            .await?;
        staged.verify()?;
        progress.stage(if reuse { 1 } else { 2 }, "Inspecting selected DVD program", None, "");
        let inspection =
            super::probe::inspect_open_iso(&staged.path, &staged.file, title, angle).await?;
        staged.verify()?;
        Ok(serde_json::json!({
            "sha256": staged_report["sha256"],
            "size_bytes": staged.size_bytes,
            "staged_attempt_id": staged_report["staged_attempt_id"],
            "inspection": inspection,
        }))
    }
    .await;
    match result {
        Ok(report) => {
            sqlx::query("UPDATE probe_jobs SET report=? WHERE id=?")
                .bind(report.to_string())
                .bind(&id)
                .execute(pool)
                .await?;
            wetube_backend::queue::defer_finish(pool, "probe", &id, "needs_review").await?;
        }
        Err(error) => {
            let message: String = format!("{error:#}").chars().take(4096).collect();
            sqlx::query("UPDATE probe_jobs SET error = ? WHERE id = ?")
                .bind(message)
                .bind(&id)
                .execute(pool)
                .await?;
            wetube_backend::queue::defer_finish(pool, "probe", &id, "failed").await?;
        }
    }
    recording.finish().await;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn scanned_selection_rehashes_the_same_staged_input_without_recopying_nas() -> Result<()>
    {
        use wetube_core::{InspectRequest, ScanRequest};
        let root = tempfile::tempdir()?;
        let state = root.path().join("state");
        let incoming = root.path().join("incoming");
        let scratch = root.path().join("scratch");
        std::fs::create_dir(&state)?;
        std::fs::create_dir(&incoming)?;
        std::fs::write(incoming.join("movie.iso"), b"original invalid DVD")?;
        let config = Arc::new(ImportConfig::open(
            &incoming,
            &scratch,
            1024,
            1024 * 1024,
            Duration::from_secs(30),
        )?);
        let confirmed = config.incoming.open_file("movie.iso")?.1;
        wetube_backend::db::initialize(&state).await?;
        let pool = wetube_backend::db::connect(&state).await?;
        sqlx::query("INSERT INTO users VALUES('admin','admin','hash','admin')")
            .execute(&pool)
            .await?;
        let scan = wetube_backend::scans::enqueue(
            &pool,
            "admin",
            &ScanRequest {
                name: confirmed.name.clone(),
                identity: confirmed.identity.clone(),
                copy_complete: true,
            },
        )
        .await?
        .context("scan not queued")?;
        let attempt = uuid::Uuid::new_v4().to_string();
        let staged_path = scratch.join(&attempt);
        std::fs::create_dir(&staged_path)?;
        std::fs::write(staged_path.join("input.iso"), b"original invalid DVD")?;
        File::open(staged_path.join("input.iso"))?.sync_all()?;
        let checksum = hex::encode(Sha256::digest(b"original invalid DVD"));
        let report = serde_json::json!({"titles":[{"title":4,"duration_seconds":30.0,"chapter_count":1,"audio_count":0,"subtitle_count":0,
            "audio_languages":[],"subtitle_languages":[],"angles":[1],"main_feature_candidate":true}]});
        sqlx::query("UPDATE scan_jobs SET state='needs_review',attempt_id=?,sha256=?,size_bytes=?,report=? WHERE id=?")
            .bind(&attempt).bind(checksum).bind(b"original invalid DVD".len() as i64).bind(report.to_string()).bind(&scan.id).execute(&pool).await?;
        std::fs::write(
            incoming.join("movie.iso"),
            b"changed NAS copy: never stage it for this selection",
        )?;
        let selected = wetube_backend::scans::inspect(
            &pool,
            "admin",
            &scan.id,
            &InspectRequest { title: 4, angle: 1 },
        )
        .await?
        .context("selection not queued")?;
        assert!(tick(&pool, config.clone()).await?);
        let failed = wetube_backend::probe_jobs::list(&pool).await?.remove(0);
        assert_eq!(failed.id, selected.id);
        assert_eq!(failed.state, wetube_core::ProbeState::Failed);
        assert!(
            failed.error.as_ref().unwrap().contains("DVD inspection"),
            "{:?}",
            failed.error
        );
        assert_eq!(
            failed.report.as_ref().unwrap()["staged_attempt_id"],
            attempt
        );
        assert_eq!(
            std::fs::read(scratch.join(&attempt).join("input.iso"))?,
            b"original invalid DVD"
        );
        assert_eq!(
            failed.attempt_id.as_deref(),
            Some(attempt.as_str()),
            "selection must reuse the scan attempt"
        );
        assert_eq!(
            std::fs::read_dir(&scratch)?
                .filter_map(|entry| entry.ok())
                .filter(|entry| entry.path().is_dir())
                .count(),
            1,
            "selection must not allocate another staging copy"
        );
        let revalidated = revalidate(&config, &failed)?;
        revalidated.verify()?;
        drop(revalidated);
        std::fs::write(
            scratch.join(&attempt).join("input.iso"),
            b"tampered invalid DVD",
        )?;
        assert!(
            revalidate(&config, &failed).is_err(),
            "fresh metadata alone cannot authorize changed content"
        );
        pool.close().await;
        Ok(())
    }

    #[test]
    fn rejects_a_replaced_verified_staging_file() {
        let dir = tempfile::tempdir().unwrap();
        let incoming = dir.path().join("incoming");
        let scratch = dir.path().join("scratch");
        std::fs::create_dir(&incoming).unwrap();
        std::fs::write(incoming.join("movie.iso"), b"verified copy").unwrap();
        let config =
            ImportConfig::open(&incoming, &scratch, 1024, 4096, Duration::from_secs(30)).unwrap();
        let confirmed = config.incoming.open_file("movie.iso").unwrap().1;
        let staged = stage(&config, &confirmed.name, &confirmed.identity, "attempt").unwrap();
        staged.verify().unwrap();
        std::fs::rename(&staged.path, staged.path.with_file_name("retained.iso")).unwrap();
        std::fs::write(&staged.path, b"verified copy").unwrap();
        assert!(staged.verify().is_err());
    }

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
        let mut staged = stage(&config, "movie.iso", &expected, "attempt").unwrap();
        let mut bytes = Vec::new();
        staged.file.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"private ISO copy");
        assert!(staged.file.write_all(b"unexpected write").is_err());
        staged.verify().unwrap();
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
    fn accepts_shared_scratch_but_refuses_symlink_roots() {
        use std::os::unix::fs::PermissionsExt;
        let incoming = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let scratch = dir.path().join("scratch");
        std::fs::create_dir(&scratch).unwrap();
        std::fs::set_permissions(&scratch, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(
            ImportConfig::open(
                incoming.path(),
                &scratch,
                100,
                100,
                Duration::from_secs(600)
            )
            .is_ok()
        );
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
        let state = dir.path().join("state");
        let incoming = dir.path().join("incoming");
        let scratch = dir.path().join("scratch");
        std::fs::create_dir(&state).unwrap();
        std::fs::create_dir(&incoming).unwrap();
        std::fs::write(incoming.join("movie.iso"), b"not a DVD").unwrap();
        let config = Arc::new(
            ImportConfig::open(&incoming, &scratch, 1024, 4096, Duration::from_secs(600)).unwrap(),
        );
        db::initialize(&state).await.unwrap();
        let pool = db::connect(&state).await.unwrap();
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
        sqlx::query("CREATE TRIGGER reject_probe_progress BEFORE UPDATE OF progress ON probe_jobs BEGIN SELECT RAISE(FAIL,'injected telemetry failure'); END")
            .execute(&pool).await.unwrap();
        assert!(tick(&pool, config.clone()).await.unwrap());
        let failed = probe_jobs::list(&pool).await.unwrap().remove(0);
        assert_eq!(failed.state, ProbeState::Failed);
        assert!(failed.progress.is_none());
        let error = failed.error.unwrap();
        assert!(error.contains("DVD inspection"), "{error}");
        assert!(failed.report.as_ref().unwrap()["sha256"].is_string());
        let retained = scratch.join(failed.attempt_id.unwrap()).join("input.iso");
        assert_eq!(std::fs::read(&retained).unwrap(), b"not a DVD");
        assert_eq!(
            std::fs::read(incoming.join("movie.iso")).unwrap(),
            b"not a DVD"
        );

        let second = probe_jobs::enqueue(&pool, "admin", &request)
            .await
            .unwrap()
            .unwrap();
        std::fs::write(incoming.join("movie.iso"), b"changed copy").unwrap();
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
