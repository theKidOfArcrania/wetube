//! Explicit local preview admission. Never publishes production assets or cleans sources.
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;
use std::{
    fs::{File, OpenOptions},
    io::{Read, Seek, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
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

fn copy_and_hash(
    mut source: File,
    mut target: File,
    size: u64,
    timeout: Duration,
) -> Result<String> {
    let deadline = Instant::now() + timeout;
    let mut digest = Sha256::new();
    let mut count = 0u64;
    let mut buffer = [0u8; 128 * 1024];
    loop {
        ensure!(Instant::now() < deadline, "preview copy deadline exceeded");
        let bytes = source.read(&mut buffer)?;
        if bytes == 0 {
            break;
        }
        count = count.checked_add(bytes as u64).context("copy overflow")?;
        ensure!(count <= size, "movie changed during copy");
        target.write_all(&buffer[..bytes])?;
        digest.update(&buffer[..bytes]);
    }
    ensure!(count == size, "movie changed during copy");
    target.sync_all()?;
    Ok(hex::encode(digest.finalize()))
}

fn hash_copy(mut file: File, size: u64, timeout: Duration) -> Result<String> {
    file.rewind()?;
    let deadline = Instant::now() + timeout;
    let mut digest = Sha256::new();
    let mut count = 0u64;
    let mut buffer = [0u8; 128 * 1024];
    loop {
        ensure!(Instant::now() < deadline, "preview hash deadline exceeded");
        let bytes = file.read(&mut buffer)?;
        if bytes == 0 {
            break;
        }
        count = count.checked_add(bytes as u64).context("hash overflow")?;
        ensure!(count <= size, "validated copy changed");
        digest.update(&buffer[..bytes]);
    }
    ensure!(count == size, "validated copy changed");
    Ok(hex::encode(digest.finalize()))
}

pub async fn admit(pool: &SqlitePool, root: &PreviewRoot, options: Admission<'_>) -> Result<Value> {
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
    let report_file = input(options.report)?;
    let report_identity = identity(&report_file.metadata()?);
    let mut report_bytes = Vec::new();
    report_file
        .try_clone()?
        .take(2 * 1024 * 1024 + 1)
        .read_to_end(&mut report_bytes)?;
    ensure!(report_bytes.len() <= 2 * 1024 * 1024, "report too large");
    let report: Value = serde_json::from_slice(&report_bytes)?;
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
    let source_path = parent.join("movie.mp4");
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
    let staging_name = format!(".admitting-{package}");
    let staging = root.descriptor_path().join(&staging_name);
    std::fs::DirBuilder::new().mode(0o700).create(&staging)?;
    // Failure leaves an unregistered hidden staging package for explicit operator inspection.
    let target = staging.join("movie.mp4");
    let source_reader = source.try_clone()?;
    let target_writer = output(&target)?;
    let timeout = options.timeout;
    let sha256 = tokio::task::spawn_blocking(move || {
        copy_and_hash(source_reader, target_writer, size, timeout)
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
    let validated = wetube_worker::preview::validate(&copied, &report, options.timeout).await?;
    // Rehash the actual validated copy before finalization.
    let hash_file = copied.try_clone()?;
    let actual_hash =
        tokio::task::spawn_blocking(move || hash_copy(hash_file, size, timeout)).await??;
    ensure!(
        actual_hash == sha256
            && identity(&copied.metadata()?) == copy_identity
            && identity(&input(&target)?.metadata()?) == copy_identity,
        "validated copy changed"
    );
    ensure!(
        identity(&report_file.metadata()?) == report_identity
            && identity(&input(options.report)?.metadata()?) == report_identity
            && identity(&source.metadata()?) == before
            && identity(&input(&source_path)?.metadata()?) == before,
        "experimental attempt changed during admission"
    );
    let mut retained_report = output(&staging.join("experimental-report.json"))?;
    retained_report.write_all(&report_bytes)?;
    retained_report.sync_all()?;
    drop(retained_report);
    let manifest = json!({
        "local_preview": true, "production_published": false, "source_cleanup": false,
        "package_id": package, "experimental_attempt_id": attempt.to_string(),
        "movie_sha256": sha256, "movie_size_bytes": size,
        "report_sha256": hex::encode(Sha256::digest(&report_bytes)),
        "duration_seconds": validated.duration_seconds, "decoded_frames": validated.decoded_frames,
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
    root.finalize(&staging_name, &package)?;
    let final_identity = identity(&root.open_movie(&package)?.metadata()?);
    ensure!(
        final_identity == copy_identity,
        "final preview payload changed"
    );
    let movie_id = uuid::Uuid::new_v4().to_string();
    let mut transaction = pool.begin().await?;
    sqlx::query("INSERT INTO movies (id,title,year,description,status) VALUES (?,?,?,?,'ready')")
        .bind(&movie_id)
        .bind(options.title.trim())
        .bind(options.year)
        .bind(options.description)
        .execute(&mut *transaction)
        .await?;
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
    transaction.commit().await?;
    Ok(json!({"movie_id": movie_id, "package_id": package, "local_preview": true}))
}
