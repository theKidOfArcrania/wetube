//! Explicit local preview admission. Never publishes production assets or cleans sources.
use crate::progress::Progress;
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{Row, SqlitePool};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Seek, Write},
    os::unix::fs::OpenOptionsExt,
    path::Path,
    time::{Duration, Instant},
};
use wetube_backend::{incoming::identity, now, preview::PreviewRoot};

pub struct Admission<'a> {
    pub report: &'a Path,
    pub title: &'a str,
    pub year: Option<i32>,
    pub description: &'a str,
    pub movie_sha256: &'a str,
    pub max_bytes: u64,
    pub timeout: Duration,
}

fn input(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    ensure!(file.metadata()?.is_file(), "expected regular input file");
    Ok(file)
}

fn output(path: &Path) -> Result<File> {
    Ok(OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?)
}

fn copy_and_hash_progress(
    mut source: File,
    mut target: File,
    size: u64,
    timeout: Duration,
    progress: Option<&Progress>,
) -> Result<String> {
    let deadline = Instant::now() + timeout;
    let mut digest = Sha256::new();
    let mut count = 0u64;
    let mut buffer = [0u8; 128 * 1024];
    loop {
        if let Some(progress) = progress {
            progress.check()?;
        }
        ensure!(Instant::now() < deadline, "preview copy deadline exceeded");
        let bytes = source.read(&mut buffer)?;
        if bytes == 0 {
            break;
        }
        count = count.checked_add(bytes as u64).context("copy overflow")?;
        ensure!(count <= size, "movie changed during copy");
        target.write_all(&buffer[..bytes])?;
        digest.update(&buffer[..bytes]);
        if let Some(progress) = progress {
            progress.units(count as f64);
        }
    }
    ensure!(count == size, "movie changed during copy");
    target.sync_all()?;
    Ok(hex::encode(digest.finalize()))
}

fn hash_copy_progress(
    mut file: File,
    size: u64,
    timeout: Duration,
    progress: Option<&Progress>,
) -> Result<String> {
    file.rewind()?;
    let deadline = Instant::now() + timeout;
    let mut digest = Sha256::new();
    let mut count = 0u64;
    let mut buffer = [0u8; 128 * 1024];
    loop {
        if let Some(progress) = progress {
            progress.check()?;
        }
        ensure!(Instant::now() < deadline, "preview hash deadline exceeded");
        let bytes = file.read(&mut buffer)?;
        if bytes == 0 {
            break;
        }
        count = count.checked_add(bytes as u64).context("hash overflow")?;
        ensure!(count <= size, "validated copy changed");
        digest.update(&buffer[..bytes]);
        if let Some(progress) = progress {
            progress.units(count as f64);
        }
    }
    ensure!(count == size, "validated copy changed");
    Ok(hex::encode(digest.finalize()))
}

pub async fn admit(pool: &SqlitePool, root: &PreviewRoot, options: Admission<'_>) -> Result<Value> {
    admit_inner(pool, root, options, None, None).await
}

pub(crate) async fn admit_conversion(
    pool: &SqlitePool,
    root: &PreviewRoot,
    options: Admission<'_>,
    job_id: &str,
    progress: &Progress,
) -> Result<Value> {
    admit_inner(pool, root, options, Some(job_id), Some(progress)).await
}

async fn admit_inner(
    pool: &SqlitePool,
    root: &PreviewRoot,
    options: Admission<'_>,
    job_id: Option<&str>,
    progress: Option<&Progress>,
) -> Result<Value> {
    wetube_backend::db::check_version(pool).await?;
    let databases = sqlx::query_as::<_, (i64, String, String)>("PRAGMA database_list")
        .fetch_all(pool)
        .await?;
    for (_, _, file) in databases {
        if !file.is_empty() {
            root.require_separate(Path::new(&file).parent().context("invalid database path")?)?;
        }
    }
    ensure!(
        !options.title.trim().is_empty()
            && options.title.len() <= 512
            && options.description.len() <= 16384,
        "invalid movie metadata"
    );
    ensure!(
        options.max_bytes > 0
            && options.max_bytes <= 64 * 1024 * 1024 * 1024
            && options.timeout > Duration::ZERO
            && options.timeout <= Duration::from_secs(86400),
        "invalid preview resource limits"
    );
    ensure!(
        options.movie_sha256.len() == 64
            && options.movie_sha256.bytes().all(|n| n.is_ascii_hexdigit()),
        "provide the operator-confirmed SHA256 digest of movie.mp4"
    );
    let parent = options
        .report
        .parent()
        .context("report requires attempt directory")?;
    root.require_separate(parent)?;
    let attempt_directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(parent)?;
    let pinned_parent = std::path::PathBuf::from(format!(
        "/proc/self/fd/{}",
        std::os::fd::AsRawFd::as_raw_fd(&attempt_directory)
    ));
    let pinned_report = pinned_parent.join(
        options
            .report
            .file_name()
            .context("report requires filename")?,
    );
    let report_file = input(&pinned_report)?;
    let report_identity = identity(&report_file.metadata()?);
    let mut report_bytes = Vec::new();
    report_file
        .try_clone()?
        .take(2 * 1024 * 1024 + 1)
        .read_to_end(&mut report_bytes)?;
    ensure!(report_bytes.len() <= 2 * 1024 * 1024, "report too large");
    let report: Value = serde_json::from_slice(&report_bytes)?;
    if let Some(id) = job_id {
        let bound: (String, String) = sqlx::query_as(
            "SELECT report,movie_sha256 FROM conversion_jobs WHERE id=? AND state='publishing'",
        )
        .bind(id)
        .fetch_one(pool)
        .await?;
        ensure!(
            serde_json::from_str::<Value>(&bound.0)? == report && bound.1 == options.movie_sha256,
            "publication report/hash differs from successful durable conversion"
        );
    }
    ensure!(
        report["experimental"] == true
            && report["published"] == false
            && report["source_deletion_enabled"] == false
            && report["status"] == "verified_experimental_output"
            && report.get("error").is_none(),
        "requires a successful complete experimental report"
    );
    let attempt = uuid::Uuid::parse_str(
        report["attempt_id"]
            .as_str()
            .context("missing attempt ID")?,
    )?;
    ensure!(
        parent.file_name().and_then(|n| n.to_str()) == Some(attempt.to_string().as_str()),
        "report attempt directory mismatch"
    );
    let source_path = pinned_parent.join("movie.mp4");
    let source = input(&source_path)?;
    let before = identity(&source.metadata()?);
    let size = source.metadata()?.len();
    ensure!(
        size > 0 && size <= options.max_bytes,
        "movie size exceeds preview bound"
    );
    ensure!(
        fs2::available_space(root.path())?
            >= size
                .checked_add(16 * 1024 * 1024)
                .context("capacity overflow")?,
        "insufficient preview capacity"
    );
    let package = uuid::Uuid::new_v4().to_string();
    // DrvFS/v9fs does not support renameat2(RENAME_NOREPLACE). Reserve the
    // UUID directory exclusively, create every file without overwrite, and
    // publish only through the final atomic SQLite registry transaction.
    let package_directory = root.reserve_package(&package)?;
    super::coordinator::record_media_package(&package);
    let staging = std::path::PathBuf::from(format!(
        "/proc/self/fd/{}",
        std::os::fd::AsRawFd::as_raw_fd(&package_directory)
    ));
    // Failure leaves an unregistered package for explicit operator inspection.
    let target = staging.join("movie.mp4");
    let source_reader = source.try_clone()?;
    let target_writer = output(&target)?;
    let timeout = options.timeout;
    let has_tracks = !report["validation"]["original_tracks"].is_null();
    let portions = if has_tracks { 5 } else { 3 };
    let movie_copy = progress.map(|p| p.substep(5, 0, portions));
    let movie_validation = progress.map(|p| p.substep(5, 1, portions));
    let movie_verification = progress.map(|p| p.substep(5, 2, portions));
    if let Some(progress) = movie_copy.as_ref() {
        progress.stage(5, "Copying and hashing movie", Some(size as f64), "bytes");
    }
    let copying_progress = movie_copy;
    let sha256 = tokio::task::spawn_blocking(move || {
        copy_and_hash_progress(
            source_reader,
            target_writer,
            size,
            timeout,
            copying_progress.as_ref(),
        )
    })
    .await??;
    ensure!(
        sha256.eq_ignore_ascii_case(options.movie_sha256),
        "movie payload does not match operator-confirmed digest"
    );
    ensure!(
        identity(&source.metadata()?) == before
            && identity(&input(&source_path)?.metadata()?) == before,
        "experimental movie changed during admission"
    );
    // Some filesystems finalize write timestamps only when the last writer closes.
    let copied = input(&target)?;
    let copy_identity = identity(&copied.metadata()?);
    if let Some(progress) = movie_validation.as_ref() {
        progress.stage(5, "Independently decoding published copy", None, "");
    }
    let validated = crate::preview::validate_progress(
        &copied,
        &report,
        options.timeout,
        movie_validation.as_ref(),
    )
    .await?;
    // Rehash the actual validated copy before finalization.
    let hash_file = copied.try_clone()?;
    if let Some(progress) = movie_verification.as_ref() {
        progress.stage(
            5,
            "Verifying published movie checksum",
            Some(size as f64),
            "bytes",
        );
    }
    let hashing_progress = movie_verification;
    let actual_hash = tokio::task::spawn_blocking(move || {
        hash_copy_progress(hash_file, size, timeout, hashing_progress.as_ref())
    })
    .await??;
    ensure!(
        actual_hash == sha256
            && identity(&copied.metadata()?) == copy_identity
            && identity(&input(&target)?.metadata()?) == copy_identity,
        "validated copy changed"
    );
    ensure!(
        identity(&report_file.metadata()?) == report_identity
            && identity(&input(&pinned_report)?.metadata()?) == report_identity
            && identity(&source.metadata()?) == before
            && identity(&input(&source_path)?.metadata()?) == before,
        "experimental attempt changed during admission"
    );
    let mut retained_report = output(&staging.join("experimental-report.json"))?;
    retained_report.write_all(&report_bytes)?;
    retained_report.sync_all()?;
    drop(retained_report);
    // Preservation assets belong to the package, not merely the processing scratch root.
    let mut original_tracks_manifest = Value::Null;
    let mut tracks_identities = None;
    if has_tracks {
        let tracks_copy = progress.map(|p| p.substep(5, 3, portions));
        let tracks_verification = progress.map(|p| p.substep(5, 4, portions));
        let tracks_path = pinned_parent.join("original-tracks.mkv");
        let tracks = input(&tracks_path)?;
        let tracks_identity = identity(&tracks.metadata()?);
        let tracks_size = tracks.metadata()?.len();
        ensure!(
            tracks_size > 0 && tracks_size <= options.max_bytes,
            "original tracks exceed publication bound"
        );
        ensure!(
            fs2::available_space(root.path())?
                >= tracks_size
                    .checked_add(16 * 1024 * 1024)
                    .context("capacity overflow")?,
            "insufficient preservation asset capacity"
        );
        let target_path = staging.join("original-tracks.mkv");
        let source_reader = tracks.try_clone()?;
        let target_writer = output(&target_path)?;
        if let Some(progress) = tracks_copy.as_ref() {
            progress.stage(
                5,
                "Copying and hashing preservation tracks",
                Some(tracks_size as f64),
                "bytes",
            );
        }
        let copying_progress = tracks_copy;
        let digest = tokio::task::spawn_blocking(move || {
            copy_and_hash_progress(
                source_reader,
                target_writer,
                tracks_size,
                timeout,
                copying_progress.as_ref(),
            )
        })
        .await??;
        if job_id.is_some() {
            ensure!(
                report["original_tracks_sha256"].as_str() == Some(digest.as_str()),
                "preservation asset checksum mismatch"
            );
        }
        ensure!(
            identity(&tracks.metadata()?) == tracks_identity
                && identity(&input(&tracks_path)?.metadata()?) == tracks_identity,
            "preservation asset changed during publication"
        );
        let copied_tracks = input(&target_path)?;
        let copied_identity = identity(&copied_tracks.metadata()?);
        let hash_file = copied_tracks.try_clone()?;
        if let Some(progress) = tracks_verification.as_ref() {
            progress.stage(
                5,
                "Verifying published preservation tracks",
                Some(tracks_size as f64),
                "bytes",
            );
        }
        let hashing_progress = tracks_verification;
        let actual = tokio::task::spawn_blocking(move || {
            hash_copy_progress(hash_file, tracks_size, timeout, hashing_progress.as_ref())
        })
        .await??;
        ensure!(
            actual == digest
                && identity(&copied_tracks.metadata()?) == copied_identity
                && identity(&input(&target_path)?.metadata()?) == copied_identity,
            "copied preservation asset changed"
        );
        original_tracks_manifest =
            json!({"file":"original-tracks.mkv","sha256":digest,"size_bytes":tracks_size});
        tracks_identities = Some((tracks_identity, copied_identity));
    }
    let manifest = json!({
        "local_preview": true, "production_published": false, "source_cleanup": false,
        "package_id": package, "experimental_attempt_id": attempt.to_string(),
        "movie_sha256": sha256, "movie_size_bytes": size,
        "report_sha256": hex::encode(Sha256::digest(&report_bytes)),
        "duration_seconds": validated.duration_seconds, "decoded_frames": validated.decoded_frames,
        "original_tracks": original_tracks_manifest,
        "conversion_job_id": job_id,
    });
    let mut manifest_file = output(&staging.join("manifest.json"))?;
    let manifest_bytes = serde_json::to_vec_pretty(&manifest)?;
    manifest_file.write_all(&manifest_bytes)?;
    manifest_file.sync_all()?;
    drop(manifest_file);
    let mut checksum = output(&staging.join("manifest.sha256"))?;
    checksum.write_all(hex::encode(Sha256::digest(&manifest_bytes)).as_bytes())?;
    checksum.sync_all()?;
    drop(checksum);
    File::open(&staging)?.sync_all()?;
    root.sync()?;
    let final_identity = identity(&root.open_movie(&package)?.metadata()?);
    ensure!(
        final_identity == copy_identity,
        "final preview payload changed"
    );
    let movie_id = uuid::Uuid::new_v4().to_string();
    if let Some(progress) = progress {
        progress.stage(6, "Durably registering verified package", None, "");
    }
    if let Some(progress) = progress {
        progress.check()?;
    }
    let mut transaction = pool.begin_with("BEGIN IMMEDIATE").await?;
    register_movie(&mut transaction, job_id, &movie_id, &options).await?;
    sqlx::query("INSERT INTO preview_media VALUES (?,?,?,?,?,?,?)")
        .bind(&movie_id)
        .bind(&package)
        .bind(&sha256)
        .bind(size as i64)
        .bind(validated.duration_seconds)
        .bind(final_identity)
        .bind(now())
        .execute(&mut *transaction)
        .await?;
    root.record_outputs(&mut transaction, &movie_id, "media", &package)
        .await?;
    let scratch = PreviewRoot::open(&pinned_parent.join(".."))?;
    scratch
        .record_outputs(&mut transaction, &movie_id, "scratch", &attempt.to_string())
        .await?;
    PreviewRoot::verify_recorded_outputs(
        &mut transaction,
        &movie_id,
        "media",
        &copy_identity,
        tracks_identities
            .as_ref()
            .map(|(_, copied)| copied.as_str()),
    )
    .await?;
    PreviewRoot::verify_recorded_outputs(
        &mut transaction,
        &movie_id,
        "scratch",
        &before,
        tracks_identities
            .as_ref()
            .map(|(source, _)| source.as_str()),
    )
    .await?;
    PreviewRoot::attest_output_hashes(
        &mut transaction,
        &movie_id,
        &sha256,
        original_tracks_manifest["sha256"].as_str(),
    )
    .await?;
    PreviewRoot::bind_output_origin(
        &mut transaction,
        &movie_id,
        &attempt.to_string(),
        job_id.is_some(),
    )
    .await?;
    transaction.commit().await?;
    Ok(json!({"movie_id": movie_id, "package_id": package, "local_preview": true}))
}

async fn register_movie(
    connection: &mut sqlx::SqliteConnection,
    job_id: Option<&str>,
    movie_id: &str,
    options: &Admission<'_>,
) -> Result<()> {
    let linked = if let Some(id) = job_id {
        let row = sqlx::query(
            "SELECT c.publication,p.name FROM conversion_jobs c JOIN probe_jobs p ON p.id=c.probe_job_id
             WHERE c.id=? AND c.state='publishing'
             AND NOT EXISTS(SELECT 1 FROM processing_queue WHERE kind='publication' AND job_id=c.id AND state IN ('cancelling','cancelled'))",
        ).bind(id).fetch_one(&mut *connection).await?;
        let publication: wetube_core::PublishRequest =
            serde_json::from_str(&row.try_get::<String, _>("publication")?)?;
        if let Some(source) = publication.tmdb {
            ensure!(
                wetube_backend::metadata::expires_at(&source)? > now(),
                "Movie metadata expired; review metadata again."
            );
            Some((
                row.get::<String, _>("name"),
                wetube_core::MetadataMatch {
                    id: source.id,
                    fetched_at: source.fetched_at,
                    title: publication.title,
                    year: publication.year,
                    description: publication.description,
                },
            ))
        } else {
            None
        }
    } else {
        None
    };
    sqlx::query("INSERT INTO movies (id,title,year,description,status) VALUES (?,?,?,?,'ready')")
        .bind(movie_id)
        .bind(
            linked
                .as_ref()
                .map_or(options.title.trim(), |(name, _)| name.as_str()),
        )
        .bind(if linked.is_some() { None } else { options.year })
        .bind(if linked.is_some() {
            ""
        } else {
            options.description
        })
        .execute(&mut *connection)
        .await?;
    if let Some((_, metadata)) = &linked {
        wetube_backend::metadata::store(connection, movie_id, metadata).await?;
    }
    if let Some(id) = job_id {
        let updated = sqlx::query("UPDATE conversion_jobs SET state='published',phase='published',movie_id=?,publication=CASE WHEN json_extract(publication,'$.tmdb') IS NOT NULL THEN NULL ELSE publication END WHERE id=? AND state='publishing'")
            .bind(movie_id).bind(id).execute(connection).await?;
        ensure!(updated.rows_affected() == 1, "publication ownership lost");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn processing_queue_cancel_and_publication_registration_are_atomic() -> Result<()> {
        let directory = tempfile::tempdir()?;
        wetube_backend::db::initialize(directory.path()).await?;
        let pool = wetube_backend::db::connect(directory.path()).await?;
        sqlx::query("INSERT INTO users VALUES('admin','admin','hash','admin')")
            .execute(&pool)
            .await?;
        let publication = wetube_core::PublishRequest {
            title: "Kept movie".into(),
            year: None,
            description: "".into(),
            movie_sha256: "a".repeat(64),
            confirm: true,
            tmdb: None,
        };
        for id in ["cancel-first", "commit-first"] {
            sqlx::query("INSERT INTO probe_jobs(id,requested_by,name,source_identity,title,angle,state,created_at) VALUES(?,'admin','retained.iso','bound',1,1,'needs_review',1)").bind(id).execute(&pool).await?;
            sqlx::query("INSERT INTO conversion_jobs(id,probe_job_id,requested_by,state,created_at,video_treatment,publication) VALUES(?,?,'admin','needs_review',1,'preserve',?)")
                .bind(id).bind(id).bind(serde_json::to_string(&publication)?).execute(&pool).await?;
            sqlx::query("UPDATE conversion_jobs SET state='queued_publication' WHERE id=?")
                .bind(id)
                .execute(&pool)
                .await?;
        }
        let options = Admission {
            report: Path::new("unused"),
            title: "Kept movie",
            year: None,
            description: "",
            movie_sha256: "unused",
            max_bytes: 1,
            timeout: Duration::from_secs(1),
        };
        let jobs = wetube_backend::queue::snapshot(&pool).await?.jobs;
        let cancelled = &jobs[0];
        wetube_backend::queue::claim(&pool, cancelled.key)
            .await?
            .unwrap();
        assert!(wetube_backend::queue::cancel(&pool, cancelled.key).await?);
        let mut transaction = pool.begin_with("BEGIN IMMEDIATE").await?;
        assert!(
            register_movie(
                &mut transaction,
                Some(&cancelled.id),
                "must-not-exist",
                &options
            )
            .await
            .is_err()
        );
        transaction.rollback().await?;
        wetube_backend::queue::finish(&pool, cancelled.key).await?;
        let committed = &jobs[1];
        wetube_backend::queue::claim(&pool, committed.key)
            .await?
            .unwrap();
        let mut transaction = pool.begin_with("BEGIN IMMEDIATE").await?;
        register_movie(&mut transaction, Some(&committed.id), "kept", &options).await?;
        transaction.commit().await?;
        assert!(!wetube_backend::queue::cancel(&pool, committed.key).await?);
        let _owner = wetube_backend::lock::MaintenanceLock::acquire(directory.path())?;
        super::super::coordinator::recover(&pool).await?;
        super::super::coordinator::recover(&pool).await?;
        assert!(!wetube_backend::queue::cancel(&pool, committed.key).await?);
        let mut transaction = pool.begin_with("BEGIN IMMEDIATE").await?;
        assert!(
            register_movie(&mut transaction, Some(&committed.id), "duplicate", &options)
                .await
                .is_err()
        );
        transaction.rollback().await?;
        assert_eq!(
            sqlx::query_scalar::<_, String>("SELECT id FROM movies")
                .fetch_one(&pool)
                .await?,
            "kept"
        );
        assert_eq!(
            wetube_backend::conversions::list(&pool)
                .await?
                .iter()
                .find(|job| job.id == cancelled.id)
                .unwrap()
                .state,
            wetube_core::ConversionState::Cancelled
        );
        pool.close().await;
        Ok(())
    }
    use wetube_core::{PublishRequest, TmdbSource};

    #[tokio::test]
    async fn metadata_registration_is_atomic_expiry_checked_and_keeps_provider_text_out_of_fallback()
    -> Result<()> {
        let directory = tempfile::tempdir()?;
        wetube_backend::db::initialize(directory.path()).await?;
        let pool = wetube_backend::db::connect(directory.path()).await?;
        let fetched_at = now();
        sqlx::query("INSERT INTO users VALUES('admin','admin','hash','admin')")
            .execute(&pool)
            .await?;
        for id in ["linked", "expired"] {
            sqlx::query("INSERT INTO probe_jobs(id,requested_by,name,source_identity,title,angle,state,created_at) VALUES(?,'admin','source.iso','bound',1,1,'needs_review',1)").bind(id).execute(&pool).await?;
            let publication = PublishRequest {
                title: "Reviewed provider title".into(),
                year: Some(2002),
                description: "Edited provider text".into(),
                movie_sha256: "a".repeat(64),
                confirm: true,
                tmdb: Some(TmdbSource {
                    id: 7,
                    fetched_at: if id == "expired" {
                        fetched_at - wetube_backend::metadata::CACHE_SECONDS
                    } else {
                        fetched_at
                    },
                }),
            };
            sqlx::query("INSERT INTO conversion_jobs(id,probe_job_id,requested_by,state,created_at,video_treatment,publication) VALUES(?,?,'admin','publishing',1,'preserve',?)")
                .bind(id).bind(id).bind(serde_json::to_string(&publication)?).execute(&pool).await?;
        }
        let options = Admission {
            report: Path::new("unused"),
            title: "Manual title",
            year: Some(1999),
            description: "Manual description",
            movie_sha256: "unused",
            max_bytes: 1,
            timeout: Duration::from_secs(1),
        };
        let mut transaction = pool.begin().await?;
        assert!(
            register_movie(&mut transaction, Some("expired"), "expired-movie", &options)
                .await
                .is_err()
        );
        transaction.rollback().await?;
        let mut transaction = pool.begin().await?;
        register_movie(&mut transaction, Some("linked"), "rollback-movie", &options).await?;
        transaction.rollback().await?;
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM movies")
                .fetch_one(&pool)
                .await?,
            0
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM tmdb_metadata")
                .fetch_one(&pool)
                .await?,
            0
        );
        assert_eq!(
            sqlx::query_scalar::<_, String>("SELECT state FROM conversion_jobs WHERE id='linked'")
                .fetch_one(&pool)
                .await?,
            "publishing"
        );
        let mut transaction = pool.begin().await?;
        register_movie(&mut transaction, Some("linked"), "movie", &options).await?;
        register_movie(&mut transaction, None, "manual", &options).await?;
        transaction.commit().await?;
        let row = sqlx::query("SELECT m.title,m.year,m.description,t.title provider_title,t.description provider_description,t.fetched_at FROM movies m JOIN tmdb_metadata t ON t.movie_id=m.id WHERE m.id='movie'").fetch_one(&pool).await?;
        assert_eq!(row.get::<String, _>("title"), "source.iso");
        assert!(row.get::<Option<i32>, _>("year").is_none());
        assert_eq!(row.get::<String, _>("description"), "");
        assert_eq!(
            row.get::<String, _>("provider_title"),
            "Reviewed provider title"
        );
        assert_eq!(
            row.get::<String, _>("provider_description"),
            "Edited provider text"
        );
        assert_eq!(row.get::<i64, _>("fetched_at"), fetched_at);
        let row = sqlx::query("SELECT * FROM conversion_jobs WHERE id='linked'")
            .fetch_one(&pool)
            .await?;
        assert_eq!(row.get::<String, _>("state"), "published");
        assert!(row.get::<Option<String>, _>("publication").is_none());
        let row = sqlx::query("SELECT * FROM movies WHERE id='manual'")
            .fetch_one(&pool)
            .await?;
        assert_eq!(row.get::<String, _>("title"), "Manual title");
        assert_eq!(row.get::<i32, _>("year"), 1999);
        assert_eq!(row.get::<String, _>("description"), "Manual description");
        pool.close().await;
        Ok(())
    }
}
