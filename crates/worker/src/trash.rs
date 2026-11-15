//! Purge runs only in the worker's existing maintenance/scratch-lock ownership.
use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};
use sqlx::{Row, SqlitePool};
use std::io::Read;
use wetube_backend::{
    incoming::identity,
    preview::{PreviewRoot, PurgeOutput},
};

fn digest(mut file: std::fs::File) -> Result<String> {
    let mut hash = Sha256::new();
    let mut bytes = [0; 128 * 1024];
    let size = file.metadata()?.len();
    let mut total = 0u64;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3600);
    loop {
        ensure!(
            std::time::Instant::now() < deadline,
            "legacy output verification timed out"
        );
        let count = file.read(&mut bytes)?;
        if count == 0 {
            break;
        }
        total = total
            .checked_add(count as u64)
            .context("legacy output size overflow")?;
        ensure!(total <= size, "legacy output grew during verification");
        hash.update(&bytes[..count]);
    }
    ensure!(total == size, "legacy output changed during verification");
    Ok(hex::encode(hash.finalize()))
}

fn manifest(media: &PreviewRoot, package: &str) -> Result<serde_json::Value> {
    let mut bytes = Vec::new();
    // Reports and manifests are bounded; movie payloads are streamed when hashing.
    let file = media.open_asset(package, "manifest.json")?;
    let before = identity(&file.metadata()?);
    file.take(64 * 1024 + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 64 * 1024, "manifest too large");
    let mut expected = String::new();
    media
        .open_asset(package, "manifest.sha256")?
        .take(65)
        .read_to_string(&mut expected)?;
    ensure!(
        expected == hex::encode(Sha256::digest(&bytes)),
        "manifest checksum changed"
    );
    ensure!(
        before == identity(&media.open_asset(package, "manifest.json")?.metadata()?),
        "manifest replaced"
    );
    let value: serde_json::Value = serde_json::from_slice(&bytes)?;
    ensure!(
        value["package_id"] == package && value["local_preview"] == true,
        "unrecognized package provenance"
    );
    Ok(value)
}

/// Schema 11 packages have an admission manifest but no file-ownership rows.
/// Derive provenance only from the registered payload identity and verified manifest.
async fn backfill(
    pool: &SqlitePool,
    media: &PreviewRoot,
    scratch: Option<&PreviewRoot>,
    movie: &str,
) -> Result<()> {
    let row = sqlx::query(
        "SELECT p.*,t.provenance_complete,t.provenance_manifest FROM preview_media p
        JOIN movie_trash t ON t.movie_id=p.movie_id WHERE p.movie_id=?",
    )
    .bind(movie)
    .fetch_one(pool)
    .await?;
    if row.get::<bool, _>("provenance_complete") {
        return Ok(());
    }
    let package: String = row.get("package_id");
    let saved: Option<String> = row.get("provenance_manifest");
    let value = match saved.as_deref() {
        Some(text) => serde_json::from_str(text)?,
        None => manifest(media, &package)?,
    };
    let movie_hash: String = row.get("sha256");
    ensure!(
        value["movie_sha256"].as_str() == Some(movie_hash.as_str()),
        "manifest differs from registered movie"
    );
    let attempt = value["experimental_attempt_id"]
        .as_str()
        .context("missing conversion provenance")?;
    ensure!(
        uuid::Uuid::parse_str(attempt)?.to_string() == attempt,
        "invalid conversion provenance"
    );
    let binding_changed: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM conversion_jobs WHERE movie_id=?
         AND (attempt_id IS NULL OR attempt_id!=? OR movie_sha256 IS NULL OR movie_sha256!=?))",
    )
    .bind(movie)
    .bind(attempt)
    .bind(&movie_hash)
    .fetch_one(pool)
    .await?;
    ensure!(
        !binding_changed,
        "conversion publication binding differs from registered movie"
    );
    let worker_owned: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM conversion_jobs WHERE attempt_id=?)
         OR EXISTS(SELECT 1 FROM movie_outputs WHERE movie_id=? AND origin='worker')",
    )
    .bind(attempt)
    .bind(movie)
    .fetch_one(pool)
    .await?;
    if worker_owned {
        PreviewRoot::bind_output_origin(&mut *pool.acquire().await?, movie, attempt, true).await?;
    }
    let tracks_hash = if value["original_tracks"].is_null() {
        None
    } else {
        Some(
            value["original_tracks"]["sha256"]
                .as_str()
                .context("missing preservation checksum")?,
        )
    };
    let expected_hash = |name: &str| match name {
        "movie.mp4" => Some(movie_hash.as_str()),
        "original-tracks.mkv" => tracks_hash,
        _ => None,
    };
    // Validate optional managed paths cheaply before reading any movie payload.
    // Never follow a legacy report's absolute source path or search outside this root.
    let scratch = if worker_owned {
        Some(scratch.context(
            "worker-owned conversion scratch root unavailable; retry with original root",
        )?)
    } else {
        scratch
    };
    let known_roots = sqlx::query(
        "SELECT root_path,root_identity,directory FROM movie_outputs WHERE movie_id=? AND root='scratch'",
    ).bind(movie).fetch_all(pool).await?;
    if worker_owned {
        for known in &known_roots {
            let root = scratch.context("worker-owned conversion scratch root unavailable")?;
            ensure!(
                root.matches_root_path(known.try_get("root_path")?),
                "worker-owned conversion scratch root differs; retry with original root"
            );
            root.verify_root_identity(known.try_get("root_identity")?)?;
            ensure!(
                known.try_get::<&str, _>("directory")? == attempt,
                "conversion ownership differs from bound attempt"
            );
        }
    }
    let sources = scratch
        .map(|root| root.inspect_outputs(attempt))
        .transpose()?
        .flatten();
    ensure!(
        !worker_owned
            || !known_roots.is_empty()
            || sources.as_ref().is_some_and(|files| !files.is_empty()),
        "worker-owned conversion root cannot be established from absent outputs; retry with original verified root"
    );
    let published = media
        .inspect_outputs(&package)?
        .context("published package is absent before ownership capture")?;
    ensure!(
        published.len() == 1 + usize::from(tracks_hash.is_some())
            && published
                .iter()
                .any(|(name, id)| name == "movie.mp4"
                    && *id == row.get::<String, _>("file_identity")),
        "registered movie changed or published ownership is ambiguous"
    );
    for (name, _) in published.iter().chain(sources.iter().flatten()) {
        ensure!(
            expected_hash(name).is_some(),
            "unrecognized generated asset"
        );
    }
    if saved.is_none() {
        sqlx::query("UPDATE movie_trash SET provenance_manifest=? WHERE movie_id=? AND provenance_manifest IS NULL")
            .bind(serde_json::to_string(&value)?).bind(movie).execute(pool).await?;
    }
    for (label, root, directory, files) in [
        ("media", Some(media), package.as_str(), published.as_slice()),
        (
            "scratch",
            scratch,
            attempt,
            sources.as_deref().unwrap_or_default(),
        ),
    ] {
        let Some(root) = root else {
            continue;
        };
        for (name, observed) in files {
            let hash = expected_hash(name).context("unrecognized generated asset")?;
            let stored: Option<(String,Option<String>)> = sqlx::query_as(
                "SELECT identity,sha256 FROM movie_outputs WHERE movie_id=? AND root=? AND directory=? AND name=?")
                .bind(movie).bind(label).bind(directory).bind(name).fetch_optional(pool).await?;
            if let Some((id, digest)) = stored {
                ensure!(
                    id == *observed && digest.as_deref() == Some(hash),
                    "checkpointed generated asset changed"
                );
                continue;
            }
            let verified = verified_file(root, directory, name, hash).await?;
            ensure!(
                *observed == verified,
                "generated asset changed after preflight"
            );
            let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
            root.record_verified_output(
                &mut tx,
                movie,
                label,
                directory,
                name,
                (&verified, hash, worker_owned),
            )
            .await?;
            tx.commit().await?;
        }
    }
    let note = if !worker_owned
        && (scratch.is_none()
            || sources
                .as_ref()
                .is_none_or(|files| files.len() < published.len()))
    {
        Some(
            "Absent managed legacy outputs were not selected; generated files outside managed scratch were retained without access.",
        )
    } else {
        None
    };
    sqlx::query("UPDATE movie_trash SET provenance_complete=1,retention_note=? WHERE movie_id=?")
        .bind(note)
        .bind(movie)
        .execute(pool)
        .await?;
    Ok(())
}

async fn verified_file(
    root: &PreviewRoot,
    directory: &str,
    name: &str,
    expected: &str,
) -> Result<String> {
    let file = root.open_asset(directory, name)?;
    let before = identity(&file.metadata()?);
    ensure!(
        tokio::task::spawn_blocking(move || digest(file)).await?? == expected,
        "generated asset checksum changed"
    );
    ensure!(
        identity(&root.open_asset(directory, name)?.metadata()?) == before,
        "generated asset changed during verification"
    );
    Ok(before)
}

fn purge_output(row: &sqlx::sqlite::SqliteRow) -> Result<PurgeOutput<'_>> {
    Ok(PurgeOutput {
        root_identity: row.try_get("root_identity")?,
        directory: row.try_get("directory")?,
        directory_identity: row.try_get("directory_identity")?,
        name: row.try_get("name")?,
        identity: row.try_get("identity")?,
        stable_identity: row.try_get("stable_identity")?,
        sha256: row
            .try_get::<Option<&str>, _>("sha256")?
            .context("missing attested output checksum")?,
    })
}

fn managed_boundary<'a>(
    row: &sqlx::sqlite::SqliteRow,
    media: &'a PreviewRoot,
    scratch: Option<&'a PreviewRoot>,
) -> Result<Option<&'a PreviewRoot>> {
    if row.try_get::<&str, _>("root")? == "media" {
        return Ok(Some(media));
    }
    let path: &[u8] = row.try_get("root_path")?;
    // Location selects the policy boundary; identity checks still reject replacement.
    let boundary = scratch.filter(|root| root.matches_root_path(path));
    ensure!(
        boundary.is_some() || row.try_get::<&str, _>("origin")? == "manual",
        "worker-owned conversion scratch root unavailable or differs; retry with original root"
    );
    Ok(boundary)
}

pub async fn tick(
    pool: &SqlitePool,
    media: &PreviewRoot,
    scratch: Option<&PreviewRoot>,
    at: i64,
) -> Result<bool> {
    tick_inner(pool, media, scratch, at, false).await
}

/// Start opportunistically without active leases; later starts remain protected
/// by the durable purge claim and transactionally checked per-output references.
pub(crate) async fn tick_quiescent(
    pool: &SqlitePool,
    media: &PreviewRoot,
    scratch: Option<&PreviewRoot>,
    at: i64,
) -> Result<bool> {
    tick_inner(pool, media, scratch, at, true).await
}

async fn tick_inner(
    pool: &SqlitePool,
    media: &PreviewRoot,
    scratch: Option<&PreviewRoot>,
    at: i64,
    quiescent: bool,
) -> Result<bool> {
    // The durable claim survives crashes; restore and authorization cannot cross it.
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    sqlx::query("DELETE FROM playback_leases WHERE expires_at<=?")
        .bind(at)
        .execute(&mut *tx)
        .await?;
    let busy: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM processing_queue WHERE state IN ('running','cancelling'))
        OR EXISTS(SELECT 1 FROM scan_jobs WHERE state IN('staging','scanning') OR (?=0 AND state='queued'))
        OR EXISTS(SELECT 1 FROM probe_jobs WHERE state IN('staging','probing') OR (?=0 AND state='queued'))
        OR EXISTS(SELECT 1 FROM conversion_jobs WHERE state IN('converting','publishing') OR (?=0 AND state IN ('queued','queued_publication')))
        OR EXISTS(SELECT 1 FROM comparison_jobs WHERE state='running' OR (?=0 AND state='queued'))
        OR EXISTS(SELECT 1 FROM diagnostic_jobs WHERE state='running' OR (?=0 AND state='queued'))")
        .bind(quiescent).bind(quiescent).bind(quiescent).bind(quiescent).bind(quiescent).fetch_one(&mut *tx).await?;
    if busy {
        tx.commit().await?;
        return Ok(false);
    }
    let id: Option<String> = sqlx::query_scalar(
        "UPDATE movie_trash SET purge_started_at=COALESCE(purge_started_at,?),attempts=attempts+1,next_attempt_at=?+60
         WHERE movie_id=(SELECT t.movie_id FROM movie_trash t
          WHERE t.purge_after<=? AND t.next_attempt_at<=? AND NOT EXISTS(SELECT 1 FROM playback_leases l WHERE l.movie_id=t.movie_id AND l.expires_at>?)
          ORDER BY t.attempts,t.purge_after,t.movie_id LIMIT 1) RETURNING movie_id"
    ).bind(at).bind(at).bind(at).bind(at).bind(at).fetch_optional(&mut *tx).await?;
    let Some(id) = id else {
        tx.commit().await?;
        return Ok(false);
    };
    tx.commit().await?;

    let result: Result<()> = async {
        // Provenance must survive a crash/partial unlink, including on the first legacy purge.
        // Hash large legacy files without holding SQLite's writer lock.
        backfill(pool, media, scratch, &id).await?;
        let pending = sqlx::query("SELECT * FROM movie_outputs WHERE movie_id=? AND removed=0")
            .bind(&id).fetch_all(pool).await?;
        // Reject unavailable/replaced worker roots before hashing any recovery intent.
        for row in &pending {
            if let Some(root) = managed_boundary(row, media, scratch)? {
                root.verify_root_identity(row.try_get("root_identity")?)?;
            }
        }
        let mut verified = std::collections::BTreeMap::new();
        for row in pending {
            let root: String = row.get("root");
            let key = (root.clone(),row.get::<String,_>("directory"),row.get::<String,_>("name"));
            let Some(boundary) = managed_boundary(&row, media, scratch)? else { continue; };
            let boundary = boundary.try_clone()?;
            if let Some(identity) = tokio::task::spawn_blocking(move || boundary.verify_purge_intent(&purge_output(&row)?)).await?? {
                verified.insert(key,identity);
            }
        }
        let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
        // This transaction also serializes enqueue/retry/publication against exclusivity checks.
        let outputs = sqlx::query("SELECT * FROM movie_outputs WHERE movie_id=? AND removed=0 ORDER BY root,directory,name")
            .bind(&id).fetch_all(&mut *tx).await?;
        let mut retained_unmanaged = false;
        for output in outputs {
            let Some(boundary) = managed_boundary(&output, media, scratch)? else {
                retained_unmanaged = true;
                continue;
            };
            let root: String = output.get("root");
            let directory: String = output.get("directory");
            let name: String = output.get("name");
            let shared: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM movie_outputs WHERE movie_id!=? AND root=? AND directory=? AND name=? AND removed=0)")
                .bind(&id).bind(&root).bind(&directory).bind(&name).fetch_one(&mut *tx).await?;
            if shared { continue; }
            if root == "scratch" {
                let used: bool = sqlx::query_scalar(
                    "SELECT EXISTS(SELECT 1 FROM conversion_jobs WHERE attempt_id=? AND (state!='published' OR (movie_id IS NOT NULL AND movie_id!=?)))
                     OR EXISTS(SELECT 1 FROM probe_jobs WHERE attempt_id=?)
                     OR EXISTS(SELECT 1 FROM scan_jobs WHERE attempt_id=?)
                     OR EXISTS(SELECT 1 FROM comparison_jobs WHERE attempt_id=?)"
                ).bind(&directory).bind(&id).bind(&directory).bind(&directory).bind(&directory)
                    .fetch_one(&mut *tx).await?;
                ensure!(!used, "generated output is retained by another job");
                // Include old/manual publications that predate the ownership table.
                let packages: Vec<(String,String)> = sqlx::query_as("SELECT movie_id,package_id FROM preview_media WHERE movie_id!=?")
                    .bind(&id).fetch_all(&mut *tx).await?;
                let mut retained = false;
                for (_, package) in packages {
                    let other = manifest(media, &package)?;
                    if other["experimental_attempt_id"].as_str() == Some(&directory) {
                        retained = true;
                    }

                }
                if retained { continue; }
            }
            boundary.remove_output(&purge_output(&output)?, verified.get(&(root.clone(),directory.clone(),name.clone())).map(String::as_str))?;
            sqlx::query("UPDATE movie_outputs SET removed=1 WHERE movie_id=? AND root=? AND directory=? AND name=?")
                .bind(&id).bind(&root).bind(&directory).bind(&name).execute(&mut *tx).await?;
        }
        if retained_unmanaged {
            sqlx::query("UPDATE movie_trash SET retention_note=CASE WHEN retention_note IS NULL THEN ? ELSE retention_note||' '||? END WHERE movie_id=?")
                .bind("Manual external conversion outputs were retained without access.")
                .bind("Manual external conversion outputs were retained without access.")
                .bind(&id).execute(&mut *tx).await?;
        }
        // Job/report history is retained; only the foreign-key association is detached.
        sqlx::query("UPDATE conversion_jobs SET movie_id=NULL WHERE movie_id=?").bind(&id).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO movie_purge_history SELECT m.id,m.title,t.deleted_at,?,t.retention_note FROM movies m JOIN movie_trash t ON t.movie_id=m.id WHERE m.id=?")
            .bind(at).bind(&id).execute(&mut *tx).await?;
        sqlx::query("DELETE FROM tmdb_posters WHERE tmdb_id IN(SELECT tmdb_id FROM tmdb_metadata WHERE movie_id=?)
            AND NOT EXISTS(SELECT 1 FROM tmdb_metadata WHERE tmdb_id=tmdb_posters.tmdb_id AND movie_id!=?)")
            .bind(&id).bind(&id).execute(&mut *tx).await?;
        sqlx::query("DELETE FROM tmdb_metadata WHERE movie_id=?").bind(&id).execute(&mut *tx).await?;
        sqlx::query("DELETE FROM movies WHERE id=?").bind(&id).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }.await;
    if let Err(error) = result {
        let message: String = format!("{error:#}").chars().take(1024).collect();
        sqlx::query("UPDATE movie_trash SET error=?,next_attempt_at=MAX(next_attempt_at,?+MIN(3600,60*(1 << MIN(attempts-1,6)))) WHERE movie_id=?")
            .bind(&message)
            .bind(at)
            .bind(&id)
            .execute(pool)
            .await?;
        tracing::warn!(movie_id=%id, error=%message, "movie purge retained for safe retry");
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::Write,
        os::unix::fs::{MetadataExt, OpenOptionsExt},
    };
    use wetube_backend::{db, trash};

    fn durable_fixture(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
        let mut writer = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)?;
        writer.write_all(bytes)?;
        writer.sync_all()?;
        drop(writer);
        std::fs::File::open(path.parent().context("missing fixture parent")?)?.sync_all()?;
        Ok(())
    }

    fn raw_identity(path: &std::path::Path) -> String {
        match std::fs::symlink_metadata(path) {
            Ok(m) => format!(
                "hash={} dev={} ino={} len={} mtime={}.{:09} ctime={}.{:09} nlink={}",
                identity(&m),
                m.dev(),
                m.ino(),
                m.len(),
                m.mtime(),
                m.mtime_nsec(),
                m.ctime(),
                m.ctime_nsec(),
                m.nlink()
            ),
            Err(error) => error.to_string(),
        }
    }

    struct Fixture {
        _root: tempfile::TempDir,
        pool: SqlitePool,
        media: PreviewRoot,
        scratch: PreviewRoot,
        package: String,
        attempt: String,
        trusted_samples: String,
    }

    impl Fixture {
        async fn new() -> Result<Self> {
            Self::with_layout(false).await
        }

        async fn with_layout(unmanaged: bool) -> Result<Self> {
            Self::with_ownership(unmanaged, !unmanaged).await
        }

        async fn with_ownership(unmanaged: bool, record: bool) -> Result<Self> {
            Self::with_origin(unmanaged, record, false).await
        }

        async fn with_origin(unmanaged: bool, record: bool, worker_owned: bool) -> Result<Self> {
            let root = tempfile::tempdir()?;
            let state = root.path().join("state");
            let media = root.path().join("media");
            let scratch = root.path().join("scratch");
            for path in [&state, &media, &scratch] {
                std::fs::create_dir(path)?;
            }
            std::fs::create_dir(root.path().join("other-scratch"))?;
            let generated = if unmanaged {
                let path = root.path().join("unmanaged");
                std::fs::create_dir(&path)?;
                path
            } else {
                scratch.clone()
            };
            db::initialize(&state).await?;
            let pool = db::connect(&state).await?;
            let package = uuid::Uuid::new_v4().to_string();
            let attempt = uuid::Uuid::new_v4().to_string();
            for (root, id) in [(&media, &package), (&generated, &attempt)] {
                std::fs::create_dir(root.join(id))?;
                for (name, bytes) in [
                    ("movie.mp4", b"generated".as_slice()),
                    ("original-tracks.mkv", b"tracks"),
                    ("input.iso", b"source"),
                    ("report.json", b"report"),
                    ("ffmpeg.log", b"log"),
                ] {
                    durable_fixture(&root.join(id).join(name), bytes)?;
                }
            }
            let hash = hex::encode(Sha256::digest(b"generated"));
            let manifest = serde_json::to_vec(&serde_json::json!({
                "local_preview":true,"package_id":package,"experimental_attempt_id":attempt,
                "movie_sha256":hash,"original_tracks":{"sha256":hex::encode(Sha256::digest(b"tracks"))}
            }))?;
            durable_fixture(&media.join(&package).join("manifest.json"), &manifest)?;
            durable_fixture(
                &media.join(&package).join("manifest.sha256"),
                hex::encode(Sha256::digest(&manifest)).as_bytes(),
            )?;
            let media = PreviewRoot::open(&media)?;
            let scratch = PreviewRoot::open(&scratch)?;
            let source = PreviewRoot::open(&generated)?;
            sqlx::query("INSERT INTO movies VALUES('movie','Original',2000,'Description','ready')")
                .execute(&pool)
                .await?;
            if worker_owned {
                sqlx::query("INSERT INTO users VALUES('owner','owner','hash','admin')")
                    .execute(&pool)
                    .await?;
                sqlx::query("INSERT INTO probe_jobs(id,requested_by,name,source_identity,title,angle,state,created_at)
                    VALUES('source-probe','owner','source.iso','identity',1,1,'needs_review',1)")
                    .execute(&pool).await?;
                sqlx::query("INSERT INTO conversion_jobs(id,probe_job_id,requested_by,state,created_at,video_treatment,attempt_id,movie_id,movie_sha256,report)
                    VALUES('publication','source-probe','owner','published',1,'preserve',?,'movie',?,'{}')")
                    .bind(&attempt).bind(&hash).execute(&pool).await?;
            }
            let mut trusted_samples = String::new();
            for (root, directory) in [(&media, &package), (&source, &attempt)] {
                for name in ["movie.mp4", "original-tracks.mkv"] {
                    let path = root.path().join(directory).join(name);
                    trusted_samples.push_str(&format!(
                        "before capture {}: {}\n",
                        path.display(),
                        raw_identity(&path)
                    ));
                }
            }
            sqlx::query("INSERT INTO preview_media VALUES('movie',?,?,9,1,?,1)")
                .bind(&package)
                .bind(&hash)
                .bind(identity(&media.open_movie(&package)?.metadata()?))
                .execute(&pool)
                .await?;
            if record {
                let mut tx = pool.begin().await?;
                media
                    .record_outputs(&mut tx, "movie", "media", &package)
                    .await?;
                source
                    .record_outputs(&mut tx, "movie", "scratch", &attempt)
                    .await?;
                PreviewRoot::attest_output_hashes(
                    &mut tx,
                    "movie",
                    &hash,
                    Some(&hex::encode(Sha256::digest(b"tracks"))),
                )
                .await?;
                PreviewRoot::bind_output_origin(&mut tx, "movie", &attempt, false).await?;
                tx.commit().await?;
            }
            for row in sqlx::query("SELECT root,name,identity FROM movie_outputs")
                .fetch_all(&pool)
                .await?
            {
                trusted_samples.push_str(&format!(
                    "recorded {}/{}: {}\n",
                    row.get::<String, _>("root"),
                    row.get::<String, _>("name"),
                    row.get::<String, _>("identity")
                ));
            }
            for (root, directory) in [(&media, &package), (&source, &attempt)] {
                for name in ["movie.mp4", "original-tracks.mkv"] {
                    let path = root.path().join(directory).join(name);
                    trusted_samples.push_str(&format!(
                        "after capture {}: {}\n",
                        path.display(),
                        raw_identity(&path)
                    ));
                }
            }
            Ok(Self {
                _root: root,
                pool,
                media,
                scratch,
                package,
                attempt,
                trusted_samples,
            })
        }

        async fn delete(&self) -> Result<i64> {
            assert!(trash::delete(&self.pool, "movie", 100).await?);
            Ok(100 + trash::RETENTION_SECONDS)
        }

        async fn retry_until(
            &self,
            due: i64,
            mut done: impl FnMut(&[wetube_core::TrashMovie]) -> bool,
        ) -> Result<()> {
            for attempt in 0..8 {
                if done(&trash::list(&self.pool).await?) {
                    return Ok(());
                }
                let scheduled: i64 = sqlx::query_scalar(
                    "SELECT next_attempt_at FROM movie_trash WHERE movie_id='movie'",
                )
                .fetch_one(&self.pool)
                .await?;
                tick(
                    &self.pool,
                    &self.media,
                    Some(&self.scratch),
                    (due + attempt * 60).max(scheduled),
                )
                .await?;
            }
            Ok(())
        }

        fn diagnostics(&self) -> String {
            let mut message = self.trusted_samples.clone();
            for (root, directory) in [(&self.media, &self.package), (&self.scratch, &self.attempt)]
            {
                for name in ["movie.mp4", "original-tracks.mkv"] {
                    for path in [
                        root.path().join(directory).join(name),
                        root.path().join(directory).join(".wetube-purge").join(name),
                    ] {
                        message.push_str(&format!(
                            "current {}: {}\n",
                            path.display(),
                            raw_identity(&path)
                        ));
                    }
                }
            }
            message
        }

        fn assert_sources(&self) -> Result<()> {
            for (root, id) in [(&self.media, &self.package), (&self.scratch, &self.attempt)] {
                for (name, expected) in [
                    ("input.iso", b"source".as_slice()),
                    ("report.json", b"report"),
                    ("ffmpeg.log", b"log"),
                ] {
                    assert_eq!(std::fs::read(root.path().join(id).join(name))?, expected);
                }
            }
            Ok(())
        }
    }

    #[tokio::test]
    async fn purge_waits_for_deadline_and_leases_serializes_authorization_and_keeps_shared_posters()
    -> Result<()> {
        let f = Fixture::new().await?;
        sqlx::query("INSERT INTO users VALUES('user','user','hash','viewer')")
            .execute(&f.pool)
            .await?;
        sqlx::query("INSERT INTO sessions VALUES('session','user','csrf',99999999)")
            .execute(&f.pool)
            .await?;
        let lease = trash::authorize(&f.pool, "movie", "session", None, 100)
            .await?
            .unwrap();
        let due = f.delete().await?;
        sqlx::query("UPDATE playback_leases SET expires_at=?")
            .bind(due + 120)
            .execute(&f.pool)
            .await?;
        assert!(!tick(&f.pool, &f.media, Some(&f.scratch), due - 1).await?);
        assert!(!tick(&f.pool, &f.media, Some(&f.scratch), due).await?);
        assert!(
            trash::authorize(&f.pool, "movie", "session", Some(&lease.id), due + 119)
                .await?
                .is_some()
        );
        sqlx::query("INSERT INTO movies VALUES('other','Other',NULL,'','ready')")
            .execute(&f.pool)
            .await?;
        for id in ["movie", "other"] {
            sqlx::query("INSERT INTO tmdb_metadata VALUES(?,7,'Provider',NULL,'',1,99999999)")
                .bind(id)
                .execute(&f.pool)
                .await?;
        }
        sqlx::query("INSERT INTO tmdb_posters VALUES(7,X'01','image/png',1,99999999),(8,X'02','image/png',1,99999999)")
            .execute(&f.pool)
            .await?;
        let (purged, renewed) = tokio::join!(
            tick(&f.pool, &f.media, Some(&f.scratch), due + 239),
            trash::authorize(&f.pool, "movie", "session", Some(&lease.id), due + 239)
        );
        assert!(purged?);
        assert!(renewed?.is_none());
        f.retry_until(due + 299, |entries| entries.is_empty())
            .await?;
        let remaining = trash::list(&f.pool).await?;
        assert!(remaining.is_empty(), "{remaining:?}\n{}", f.diagnostics());
        assert!(f.media.open_movie(&f.package).is_err());
        assert!(f.scratch.open_movie(&f.attempt).is_err());
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM tmdb_posters")
                .fetch_one(&f.pool)
                .await?,
            2
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM movie_purge_history")
                .fetch_one(&f.pool)
                .await?,
            1
        );
        f.assert_sources()?;
        Ok(())
    }

    #[tokio::test]
    async fn renamed_intent_requires_payload_hash_and_rechecks_full_identity_before_unlink()
    -> Result<()> {
        let f = Fixture::new().await?;
        let directory = f.media.path().join(&f.package);
        let path = directory.join("movie.mp4");
        // Whole-second fixture timestamps can be restored exactly on DrvFS.
        std::fs::File::open(&path)?.set_times(
            std::fs::FileTimes::new().set_modified(
                std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000),
            ),
        )?;
        sqlx::query("DELETE FROM movie_outputs WHERE root='media'")
            .execute(&f.pool)
            .await?;
        f.media
            .record_outputs(&mut *f.pool.acquire().await?, "movie", "media", &f.package)
            .await?;
        PreviewRoot::attest_output_hashes(
            &mut *f.pool.acquire().await?,
            "movie",
            &hex::encode(Sha256::digest(b"generated")),
            Some(&hex::encode(Sha256::digest(b"tracks"))),
        )
        .await?;
        let row = sqlx::query("SELECT * FROM movie_outputs WHERE movie_id='movie' AND root='media' AND name='movie.mp4'").fetch_one(&f.pool).await?;
        let output = purge_output(&row)?;
        let original_time = path.metadata()?.modified()?;
        std::fs::create_dir(directory.join(".wetube-purge"))?;
        let staged = directory.join(".wetube-purge/movie.mp4");
        std::fs::rename(&path, &staged)?;
        assert_eq!(
            wetube_backend::preview::stable_identity(&staged.metadata()?),
            output.stable_identity,
            "rename changed stable metadata"
        );
        // Same inode, length and restored mtime are insufficient recovery provenance.
        std::fs::write(&staged, b"malicious")?;
        std::fs::File::open(&staged)?
            .set_times(std::fs::FileTimes::new().set_modified(original_time))?;
        assert!(f.media.verify_purge_intent(&output).is_err());
        assert!(f.media.remove_output(&output, None).is_err());
        assert_eq!(std::fs::read(&staged)?, b"malicious");
        std::fs::write(&staged, b"generated")?;
        std::fs::File::open(&staged)?
            .set_times(std::fs::FileTimes::new().set_modified(original_time))?;
        let verified = f
            .media
            .verify_purge_intent(&output)?
            .context("missing recovery identity")?;
        f.media.remove_output(&output, Some(&verified))?;
        assert!(!staged.exists());
        f.assert_sources()?;
        Ok(())
    }

    #[tokio::test]
    async fn completed_file_work_finalizes_only_its_unreferenced_poster_and_retains_job_history()
    -> Result<()> {
        let f = Fixture::new().await?;
        let due = f.delete().await?;
        // Resume from the durable state after all verified file removals completed.
        for (root, directory) in [(&f.media, &f.package), (&f.scratch, &f.attempt)] {
            for name in ["movie.mp4", "original-tracks.mkv"] {
                std::fs::remove_file(root.path().join(directory).join(name))?;
            }
        }
        sqlx::query("UPDATE movie_outputs SET removed=1")
            .execute(&f.pool)
            .await?;
        sqlx::query("INSERT INTO movies VALUES('other','Other',NULL,'','ready')")
            .execute(&f.pool)
            .await?;
        sqlx::query("INSERT INTO tmdb_metadata VALUES('movie',7,'Title',NULL,'',1,99999999),('other',8,'Other',NULL,'',1,99999999)").execute(&f.pool).await?;
        sqlx::query("INSERT INTO tmdb_posters VALUES(7,X'01','image/png',1,99999999),(8,X'02','image/png',1,99999999),(9,X'03','image/png',1,99999999)").execute(&f.pool).await?;
        sqlx::query("INSERT INTO users VALUES('user','user','hash','admin')")
            .execute(&f.pool)
            .await?;
        sqlx::query("INSERT INTO probe_jobs(id,requested_by,name,source_identity,title,angle,state,created_at) VALUES('probe','user','source.iso','identity',1,1,'needs_review',1)").execute(&f.pool).await?;
        sqlx::query("INSERT INTO conversion_jobs(id,probe_job_id,requested_by,state,created_at,video_treatment,movie_id,report) VALUES('job','probe','user','published',1,'preserve','movie','{}')").execute(&f.pool).await?;
        assert!(tick(&f.pool, &f.media, Some(&f.scratch), due).await?);
        assert!(trash::list(&f.pool).await?.is_empty());
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM tmdb_posters WHERE tmdb_id IN(8,9)")
                .fetch_one(&f.pool)
                .await?,
            2
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM tmdb_posters WHERE tmdb_id=7")
                .fetch_one(&f.pool)
                .await?,
            0
        );
        let job: (String, Option<String>, String) =
            sqlx::query_as("SELECT state,movie_id,report FROM conversion_jobs")
                .fetch_one(&f.pool)
                .await?;
        assert_eq!(job, ("published".into(), None, "{}".into()));
        f.assert_sources()?;
        Ok(())
    }

    #[tokio::test]
    async fn restore_and_purge_claim_cannot_both_win() -> Result<()> {
        let f = Fixture::new().await?;
        let due = f.delete().await?;
        let (restored, purged) = tokio::join!(
            trash::restore(&f.pool, "movie", due - 1),
            tick(&f.pool, &f.media, Some(&f.scratch), due),
        );
        assert_ne!(restored?, purged?);
        if sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM movies WHERE id='movie' AND status='ready')",
        )
        .fetch_one(&f.pool)
        .await?
        {
            assert!(f.media.open_movie(&f.package).is_ok());
            assert!(f.scratch.open_movie(&f.attempt).is_ok());
        } else {
            assert!(!trash::restore(&f.pool, "movie", due - 1).await?);
        }
        f.assert_sources()?;
        Ok(())
    }

    #[tokio::test]
    async fn interrupted_partial_purge_keeps_auditable_claim_and_retries_idempotently() -> Result<()>
    {
        let f = Fixture::new().await?;
        let due = f.delete().await?;
        sqlx::query("CREATE TRIGGER fail_purge BEFORE DELETE ON movies BEGIN SELECT RAISE(ABORT,'injected finalization failure'); END").execute(&f.pool).await?;
        tick(&f.pool, &f.media, Some(&f.scratch), due).await?;
        f.retry_until(due + 60, |entries| {
            entries.first().is_some_and(|entry| {
                entry
                    .error
                    .as_ref()
                    .is_some_and(|error| error.contains("injected finalization failure"))
            })
        })
        .await?;
        let entry = trash::list(&f.pool).await?.remove(0);
        assert!(entry.purging);
        assert!(
            entry
                .error
                .as_ref()
                .unwrap()
                .contains("injected finalization failure"),
            "{entry:?}\n{}",
            f.diagnostics()
        );
        assert!(!trash::restore(&f.pool, "movie", due - 1).await?);
        assert!(f.media.open_movie(&f.package).is_err());
        sqlx::query("DROP TRIGGER fail_purge")
            .execute(&f.pool)
            .await?;
        // A new connection/root simulates restart: no in-memory claim or asset state is required.
        let media = PreviewRoot::open(f.media.path())?;
        let scratch = PreviewRoot::open(f.scratch.path())?;
        assert!(!tick(&f.pool, &media, Some(&scratch), due + 1).await?);
        let retry_at: i64 = sqlx::query_scalar("SELECT next_attempt_at FROM movie_trash")
            .fetch_one(&f.pool)
            .await?;
        tick(&f.pool, &media, Some(&scratch), retry_at).await?;
        assert!(trash::list(&f.pool).await?.is_empty());
        assert!(!tick(&f.pool, &media, Some(&scratch), retry_at + 1).await?);
        f.assert_sources()?;
        Ok(())
    }

    #[tokio::test]
    async fn legacy_provenance_is_durable_before_first_unlink() -> Result<()> {
        let f = Fixture::new().await?;
        sqlx::query("DELETE FROM movie_outputs")
            .execute(&f.pool)
            .await?;
        let due = f.delete().await?;
        sqlx::query("CREATE TRIGGER fail_purge BEFORE DELETE ON movies BEGIN SELECT RAISE(ABORT,'crash'); END").execute(&f.pool).await?;
        tick(&f.pool, &f.media, Some(&f.scratch), due).await?;
        let legacy_status = trash::list(&f.pool).await?;
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM movie_outputs")
                .fetch_one(&f.pool)
                .await?,
            4,
            "{legacy_status:?}"
        );
        sqlx::query("DROP TRIGGER fail_purge")
            .execute(&f.pool)
            .await?;
        tick(&f.pool, &f.media, Some(&f.scratch), due + 60).await?;
        f.retry_until(due + 120, |entries| entries.is_empty())
            .await?;
        let remaining = trash::list(&f.pool).await?;
        assert!(remaining.is_empty(), "{remaining:?}\n{}", f.diagnostics());
        f.assert_sources()?;
        Ok(())
    }

    #[tokio::test]
    async fn shared_generated_outputs_are_retained_and_live_job_claims_block_cleanup() -> Result<()>
    {
        let f = Fixture::new().await?;
        sqlx::query("INSERT INTO movies VALUES('other','Other',NULL,'','ready')")
            .execute(&f.pool)
            .await?;
        f.scratch
            .record_outputs(
                &mut *f.pool.acquire().await?,
                "other",
                "scratch",
                &f.attempt,
            )
            .await?;
        let due = f.delete().await?;
        tick(&f.pool, &f.media, Some(&f.scratch), due).await?;
        f.retry_until(due + 60, |entries| entries.is_empty())
            .await?;
        assert!(trash::list(&f.pool).await?.is_empty());
        assert!(f.scratch.open_movie(&f.attempt).is_ok());
        f.assert_sources()?;

        let f = Fixture::new().await?;
        let due = f.delete().await?;
        sqlx::query("INSERT INTO users VALUES('user','user','hash','admin')")
            .execute(&f.pool)
            .await?;
        sqlx::query("INSERT INTO probe_jobs(id,requested_by,name,source_identity,title,angle,state,created_at,attempt_id) VALUES('probe','user','source.iso','identity',1,1,'needs_review',1,?)").bind(&f.attempt).execute(&f.pool).await?;
        tick(&f.pool, &f.media, Some(&f.scratch), due).await?;
        f.retry_until(due + 60, |entries| {
            entries.first().is_some_and(|entry| {
                entry
                    .error
                    .as_ref()
                    .is_some_and(|error| error.contains("another job"))
            })
        })
        .await?;
        assert!(
            trash::list(&f.pool).await?[0]
                .error
                .as_ref()
                .unwrap()
                .contains("another job")
        );
        assert!(f.scratch.open_movie(&f.attempt).is_ok());
        f.assert_sources()?;
        Ok(())
    }

    #[tokio::test]
    async fn legacy_missing_or_unmanaged_attempt_does_not_block_published_purge() -> Result<()> {
        for configured in [true, false] {
            // Build the historical layout before capturing trusted published identities.
            let f = Fixture::with_layout(true).await?;
            let retained = f._root.path().join("unmanaged").join(&f.attempt);
            let due = f.delete().await?;
            tick(&f.pool, &f.media, configured.then_some(&f.scratch), due).await?;
            f.retry_until(due + 60, |entries| entries.is_empty())
                .await?;
            let remaining = trash::list(&f.pool).await?;
            assert!(remaining.is_empty(), "{remaining:?}\n{}", f.diagnostics());
            assert!(f.media.open_movie(&f.package).is_err());
            let note: String = sqlx::query_scalar(
                "SELECT retention_note FROM movie_purge_history WHERE movie_id='movie'",
            )
            .fetch_one(&f.pool)
            .await?;
            assert!(note.contains("retained without access"));
            for (name, expected) in [
                ("movie.mp4", b"generated".as_slice()),
                ("original-tracks.mkv", b"tracks"),
                ("input.iso", b"source"),
                ("report.json", b"report"),
                ("ffmpeg.log", b"log"),
            ] {
                assert_eq!(std::fs::read(retained.join(name))?, expected);
            }
            for name in ["input.iso", "report.json", "ffmpeg.log"] {
                assert!(f.media.path().join(&f.package).join(name).exists());
            }
        }
        Ok(())
    }

    async fn assert_unmanaged_scratch_retention(
        configured: bool,
        published_removed: bool,
    ) -> Result<()> {
        let f = Fixture::with_ownership(true, true).await?;
        let retained = f._root.path().join("unmanaged").join(&f.attempt);
        if published_removed {
            for name in ["movie.mp4", "original-tracks.mkv"] {
                std::fs::remove_file(f.media.path().join(&f.package).join(name))?;
            }
            sqlx::query("UPDATE movie_outputs SET removed=1 WHERE root='media'")
                .execute(&f.pool)
                .await?;
        }
        let due = f.delete().await?;
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT provenance_complete FROM movie_trash")
                .fetch_one(&f.pool)
                .await?,
            1
        );
        let mut at = due;
        for _ in 0..8 {
            tick(&f.pool, &f.media, configured.then_some(&f.scratch), at).await?;
            if trash::list(&f.pool).await?.is_empty() {
                break;
            }
            at = sqlx::query_scalar("SELECT next_attempt_at FROM movie_trash")
                .fetch_one(&f.pool)
                .await?;
        }
        let remaining = trash::list(&f.pool).await?;
        assert!(remaining.is_empty(), "{remaining:?}\n{}", f.diagnostics());
        assert!(
            !sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM movies WHERE id='movie')")
                .fetch_one(&f.pool)
                .await?
        );
        for name in ["movie.mp4", "original-tracks.mkv"] {
            assert!(!f.media.path().join(&f.package).join(name).exists());
        }
        let note: String = sqlx::query_scalar(
            "SELECT retention_note FROM movie_purge_history WHERE movie_id='movie'",
        )
        .fetch_one(&f.pool)
        .await?;
        assert!(note.contains("Manual external conversion outputs were retained without access"));
        for (name, expected) in [
            ("movie.mp4", b"generated".as_slice()),
            ("original-tracks.mkv", b"tracks"),
            ("input.iso", b"source"),
            ("report.json", b"report"),
            ("ffmpeg.log", b"log"),
        ] {
            assert_eq!(std::fs::read(retained.join(name))?, expected);
            if !matches!(name, "movie.mp4" | "original-tracks.mkv") {
                assert_eq!(
                    std::fs::read(f.media.path().join(&f.package).join(name))?,
                    expected
                );
            }
        }
        Ok(())
    }

    #[tokio::test]
    async fn recorded_scratch_outside_worker_boundary_with_no_root_is_retained() -> Result<()> {
        assert_unmanaged_scratch_retention(false, false).await
    }

    #[tokio::test]
    async fn recorded_scratch_outside_worker_boundary_with_different_root_is_retained() -> Result<()>
    {
        assert_unmanaged_scratch_retention(true, false).await
    }

    #[tokio::test]
    async fn recorded_scratch_unmanaged_partial_purge_finalizes_without_reopening_source()
    -> Result<()> {
        for configured in [false, true] {
            assert_unmanaged_scratch_retention(configured, true).await?;
        }
        Ok(())
    }

    async fn assert_worker_root_retries(legacy: bool, configured: bool) -> Result<()> {
        let f = Fixture::with_origin(false, !legacy, true).await?;
        let other = PreviewRoot::open(&f._root.path().join("other-scratch"))?;
        let due = f.delete().await?;
        let before: i64 = sqlx::query_scalar("SELECT count(*) FROM movie_outputs")
            .fetch_one(&f.pool)
            .await?;
        assert!(tick(&f.pool, &f.media, configured.then_some(&other), due).await?);
        let pending = trash::list(&f.pool).await?;
        assert!(
            pending[0].purging
                && pending[0]
                    .error
                    .as_ref()
                    .is_some_and(|error| error.contains("worker-owned conversion")),
            "{pending:?}\n{}",
            f.diagnostics()
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM movie_outputs")
                .fetch_one(&f.pool)
                .await?,
            before
        );
        assert!(
            sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS(SELECT 1 FROM conversion_jobs WHERE movie_id='movie')"
            )
            .fetch_one(&f.pool)
            .await?
        );
        for (root, directory) in [(&f.media, &f.package), (&f.scratch, &f.attempt)] {
            for (name, expected) in [
                ("movie.mp4", b"generated".as_slice()),
                ("original-tracks.mkv", b"tracks"),
            ] {
                assert_eq!(
                    std::fs::read(root.path().join(directory).join(name))?,
                    expected
                );
            }
        }
        f.assert_sources()?;
        assert!(
            sqlx::query_scalar::<_, Option<String>>("SELECT retention_note FROM movie_trash")
                .fetch_one(&f.pool)
                .await?
                .is_none()
        );
        let retry_at: i64 = sqlx::query_scalar("SELECT next_attempt_at FROM movie_trash")
            .fetch_one(&f.pool)
            .await?;
        assert!(retry_at > due);
        assert!(
            !tick(
                &f.pool,
                &f.media,
                configured.then_some(&other),
                retry_at - 1
            )
            .await?
        );
        tick(&f.pool, &f.media, Some(&f.scratch), retry_at).await?;
        f.retry_until(retry_at + 60, |entries| entries.is_empty())
            .await?;
        let remaining = trash::list(&f.pool).await?;
        assert!(remaining.is_empty(), "{remaining:?}\n{}", f.diagnostics());
        for (root, directory) in [(&f.media, &f.package), (&f.scratch, &f.attempt)] {
            for name in ["movie.mp4", "original-tracks.mkv"] {
                assert!(!root.path().join(directory).join(name).exists());
            }
        }
        assert!(
            sqlx::query_scalar::<_, Option<String>>(
                "SELECT retention_note FROM movie_purge_history"
            )
            .fetch_one(&f.pool)
            .await?
            .is_none()
        );
        f.assert_sources()?;
        Ok(())
    }

    #[tokio::test]
    async fn worker_origin_new_publication_without_root_waits_and_retries() -> Result<()> {
        assert_worker_root_retries(false, false).await
    }

    #[tokio::test]
    async fn worker_origin_new_publication_with_different_root_waits_and_retries() -> Result<()> {
        assert_worker_root_retries(false, true).await
    }

    #[tokio::test]
    async fn worker_origin_legacy_publication_without_root_waits_and_retries() -> Result<()> {
        assert_worker_root_retries(true, false).await
    }

    #[tokio::test]
    async fn worker_origin_legacy_publication_with_different_root_waits_and_retries() -> Result<()>
    {
        assert_worker_root_retries(true, true).await
    }

    #[tokio::test]
    async fn worker_origin_known_root_allows_provably_absent_optional_outputs() -> Result<()> {
        for legacy in [false, true] {
            let f = Fixture::with_origin(false, !legacy, true).await?;
            let due = f.delete().await?;
            if legacy {
                backfill(&f.pool, &f.media, Some(&f.scratch), "movie").await?;
                // Simulate interruption after checkpoints, before the completion marker.
                sqlx::query("UPDATE movie_trash SET provenance_complete=0")
                    .execute(&f.pool)
                    .await?;
            }
            for name in ["movie.mp4", "original-tracks.mkv"] {
                std::fs::remove_file(f.scratch.path().join(&f.attempt).join(name))?;
            }
            tick(&f.pool, &f.media, Some(&f.scratch), due).await?;
            f.retry_until(due + 60, |entries| entries.is_empty())
                .await?;
            let remaining = trash::list(&f.pool).await?;
            assert!(remaining.is_empty(), "{remaining:?}\n{}", f.diagnostics());
            f.assert_sources()?;
        }
        Ok(())
    }

    #[tokio::test]
    async fn legacy_preflight_refuses_symlinked_attempt_before_hashing_or_checkpointing()
    -> Result<()> {
        let f = Fixture::new().await?;
        sqlx::query("DELETE FROM movie_outputs")
            .execute(&f.pool)
            .await?;
        let retained = f._root.path().join("unmanaged");
        std::fs::rename(f.scratch.path().join(&f.attempt), &retained)?;
        std::os::unix::fs::symlink(&retained, f.scratch.path().join(&f.attempt))?;
        let due = f.delete().await?;
        tick(&f.pool, &f.media, Some(&f.scratch), due).await?;
        assert!(trash::list(&f.pool).await?[0].error.is_some());
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM movie_outputs")
                .fetch_one(&f.pool)
                .await?,
            0
        );
        assert!(
            sqlx::query_scalar::<_, Option<String>>("SELECT provenance_manifest FROM movie_trash")
                .fetch_one(&f.pool)
                .await?
                .is_none()
        );
        assert!(f.media.open_movie(&f.package).is_ok());
        assert_eq!(std::fs::read(retained.join("movie.mp4"))?, b"generated");
        Ok(())
    }

    #[tokio::test]
    async fn legacy_hash_checkpoints_survive_source_error_with_backoff_and_queued_work_priority()
    -> Result<()> {
        let f = Fixture::new().await?;
        sqlx::query("DELETE FROM movie_outputs")
            .execute(&f.pool)
            .await?;
        // This is a deliberate source mismatch, not a new trusted identity capture.
        std::fs::write(
            f.scratch.path().join(&f.attempt).join("movie.mp4"),
            b"unrelated",
        )?;
        let due = f.delete().await?;
        assert!(tick(&f.pool, &f.media, Some(&f.scratch), due).await?);
        let status = trash::list(&f.pool).await?;
        assert!(
            status[0]
                .error
                .as_ref()
                .is_some_and(|e| e.contains("checksum changed")),
            "{status:?}\n{}",
            f.diagnostics()
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM movie_outputs WHERE root='media'")
                .fetch_one(&f.pool)
                .await?,
            2
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT provenance_complete FROM movie_trash")
                .fetch_one(&f.pool)
                .await?,
            0
        );
        assert!(!tick(&f.pool, &f.media, Some(&f.scratch), due + 59).await?);
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT attempts FROM movie_trash")
                .fetch_one(&f.pool)
                .await?,
            1
        );
        assert!(tick(&f.pool, &f.media, Some(&f.scratch), due + 60).await?);
        let schedule: (i64, i64) =
            sqlx::query_as("SELECT attempts,next_attempt_at FROM movie_trash")
                .fetch_one(&f.pool)
                .await?;
        assert_eq!(schedule, (2, due + 180));
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM movie_outputs WHERE root='media'")
                .fetch_one(&f.pool)
                .await?,
            2
        );
        sqlx::query("INSERT INTO users VALUES('user','user','hash','admin')")
            .execute(&f.pool)
            .await?;
        sqlx::query("INSERT INTO probe_jobs(id,requested_by,name,source_identity,title,angle,state,created_at) VALUES('queued','user','input.iso','identity',1,1,'queued',1)").execute(&f.pool).await?;
        assert!(!tick(&f.pool, &f.media, Some(&f.scratch), due + 180).await?);
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT attempts FROM movie_trash")
                .fetch_one(&f.pool)
                .await?,
            2
        );
        assert_eq!(
            std::fs::read(f.scratch.path().join(&f.attempt).join("movie.mp4"))?,
            b"unrelated"
        );
        f.assert_sources()?;
        Ok(())
    }

    #[tokio::test]
    async fn recorded_scratch_root_replacement_is_not_treated_as_unmanaged() -> Result<()> {
        let f = Fixture::with_origin(false, true, true).await?;
        let retained = f._root.path().join("retained-scratch");
        let location = f.scratch.path().to_owned();
        // Replace storage between worker lifetimes, without changing recorded ownership.
        drop(f.scratch);
        std::fs::rename(&location, &retained).context("retain prior scratch root")?;
        std::fs::create_dir(&location).context("replace managed scratch location")?;
        let replaced = PreviewRoot::open(&location)?;
        assert!(trash::delete(&f.pool, "movie", 100).await?);
        let due = 100 + trash::RETENTION_SECONDS;
        tick(&f.pool, &f.media, Some(&replaced), due).await?;
        let status = trash::list(&f.pool).await?;
        assert!(
            status[0]
                .error
                .as_ref()
                .is_some_and(|e| e.contains("root was replaced")),
            "{status:?}"
        );
        assert!(f.media.open_movie(&f.package).is_ok());
        assert!(
            sqlx::query_scalar::<_, Option<String>>("SELECT retention_note FROM movie_trash")
                .fetch_one(&f.pool)
                .await?
                .is_none()
        );
        for (name, expected) in [
            ("movie.mp4", b"generated".as_slice()),
            ("original-tracks.mkv", b"tracks"),
            ("input.iso", b"source"),
            ("report.json", b"report"),
            ("ffmpeg.log", b"log"),
        ] {
            assert_eq!(
                std::fs::read(retained.join(&f.attempt).join(name))
                    .with_context(|| format!("read retained scratch {name}"))?,
                expected
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn recorded_scratch_file_replacement_still_fails_closed() -> Result<()> {
        let f = Fixture::with_origin(false, true, true).await?;
        // Isolate scratch checks after a completed published-file stage.
        for name in ["movie.mp4", "original-tracks.mkv"] {
            std::fs::remove_file(f.media.path().join(&f.package).join(name))?;
        }
        sqlx::query("UPDATE movie_outputs SET removed=1 WHERE root='media'")
            .execute(&f.pool)
            .await?;
        let due = f.delete().await?;
        let path = f.scratch.path().join(&f.attempt).join("movie.mp4");
        std::fs::write(&path, b"replacement")?;
        tick(&f.pool, &f.media, Some(&f.scratch), due).await?;
        let status = trash::list(&f.pool).await?;
        assert!(
            status[0]
                .error
                .as_ref()
                .is_some_and(|e| e.contains("asset identity changed")),
            "{status:?}\n{}",
            f.diagnostics()
        );
        for name in ["movie.mp4", "original-tracks.mkv"] {
            assert!(
                !f.media.path().join(&f.package).join(name).exists(),
                "{status:?}\n{}",
                f.diagnostics()
            );
        }
        assert_eq!(std::fs::read(&path)?, b"replacement");
        assert!(
            sqlx::query_scalar::<_, Option<String>>("SELECT retention_note FROM movie_trash")
                .fetch_one(&f.pool)
                .await?
                .is_none()
        );
        Ok(())
    }

    #[tokio::test]
    async fn conversion_retries_publication_scans_and_comparisons_retain_claimed_outputs()
    -> Result<()> {
        for kind in [
            "queued",
            "publishing",
            "failed",
            "published_other",
            "scan",
            "comparison",
        ] {
            let f = Fixture::new().await?;
            let due = f.delete().await?;
            // Resume after the published-file stage, so this test isolates job exclusivity.
            for name in ["movie.mp4", "original-tracks.mkv"] {
                std::fs::remove_file(f.media.path().join(&f.package).join(name))?;
            }
            sqlx::query("UPDATE movie_outputs SET removed=1 WHERE root='media'")
                .execute(&f.pool)
                .await?;
            sqlx::query("INSERT INTO users VALUES('user','user','hash','admin')")
                .execute(&f.pool)
                .await?;
            sqlx::query("INSERT INTO probe_jobs(id,requested_by,name,source_identity,title,angle,state,created_at) VALUES('probe','user','source.iso','identity',1,1,'needs_review',1)").execute(&f.pool).await?;
            match kind {
                "scan" => {
                    sqlx::query("INSERT INTO scan_jobs(id,requested_by,name,source_identity,state,created_at,attempt_id) VALUES('scan','user','source.iso','identity','queued',1,?)")
                        .bind(&f.attempt).execute(&f.pool).await?;
                }
                "comparison" => {
                    sqlx::query("INSERT INTO comparison_jobs(id,probe_job_id,requested_by,state,created_at,attempt_id,probe_report,title,angle) VALUES('comparison','probe','user','running',1,?,'{}',1,1)")
                        .bind(&f.attempt).execute(&f.pool).await?;
                }
                _ => {
                    if kind == "published_other" {
                        sqlx::query("INSERT INTO movies VALUES('other','Other',NULL,'','ready')")
                            .execute(&f.pool)
                            .await?;
                    }
                    sqlx::query("INSERT INTO conversion_jobs(id,probe_job_id,requested_by,state,created_at,video_treatment,attempt_id,movie_id) VALUES('conversion','probe','user',?,1,'preserve',?,?)")
                        .bind(if kind == "published_other" { "published" } else { kind }).bind(&f.attempt)
                        .bind(if kind == "published_other" { Some("other") } else { None }).execute(&f.pool).await?;
                }
            }
            let claimed = tick(&f.pool, &f.media, Some(&f.scratch), due).await?;
            let pending = trash::list(&f.pool).await?;
            if matches!(kind, "failed" | "published_other") {
                assert!(
                    claimed
                        && pending[0]
                            .error
                            .as_ref()
                            .is_some_and(|error| error.contains("another job")),
                    "{kind}: {pending:?}"
                );
            } else {
                assert!(
                    !claimed && !pending[0].purging && pending[0].error.is_none(),
                    "{kind}: {pending:?}"
                );
            }
            assert_eq!(
                std::fs::read(f.scratch.path().join(&f.attempt).join("movie.mp4"))?,
                b"generated"
            );
            f.assert_sources()?;
        }
        Ok(())
    }

    #[tokio::test]
    async fn unsafe_changed_hardlinked_and_replaced_paths_are_never_deleted() -> Result<()> {
        for kind in [
            "symlink",
            "hardlink",
            "changed",
            "directory",
            "traversal",
            "intent",
        ] {
            let f = Fixture::new().await?;
            let due = f.delete().await?;
            let directory = f.media.path().join(&f.package);
            let payload = directory.join("movie.mp4");
            let protected = f.media.path().join("protected.iso");
            std::fs::write(&protected, b"original source")?;
            match kind {
                "symlink" => {
                    std::fs::remove_file(&payload)?;
                    std::os::unix::fs::symlink(&protected, &payload)?;
                }
                "hardlink" => {
                    std::fs::hard_link(&payload, directory.join("unrelated.mp4"))?;
                }
                "changed" => std::fs::write(&payload, b"replacement")?,
                "directory" => {
                    std::fs::rename(&directory, f.media.path().join("retained"))?;
                    std::fs::create_dir(&directory)?;
                    std::fs::write(&payload, b"replacement")?;
                }
                "traversal" => {
                    sqlx::query(
                        "UPDATE movie_outputs SET directory='../outside' WHERE root='media'",
                    )
                    .execute(&f.pool)
                    .await?;
                }
                "intent" => {
                    std::fs::create_dir(directory.join(".wetube-purge"))?;
                    std::fs::write(directory.join(".wetube-purge/movie.mp4"), b"unrelated")?;
                }
                _ => unreachable!(),
            }
            tick(&f.pool, &f.media, Some(&f.scratch), due).await?;
            assert!(!trash::list(&f.pool).await?.is_empty(), "{kind}");
            assert!(trash::list(&f.pool).await?[0].error.is_some(), "{kind}");
            assert_eq!(std::fs::read(&protected)?, b"original source", "{kind}");
            assert!(payload.symlink_metadata().is_ok(), "{kind}");
        }
        Ok(())
    }
}
