use anyhow::{Context, Result, ensure};
use sqlx::{Row, SqlitePool};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{sync::Notify, task::JoinSet};
use wetube_backend::{preview::PreviewRoot, queue};

#[derive(Clone, Default)]
pub(crate) struct Cancellation(Arc<(AtomicBool, Notify)>);

impl Cancellation {
    pub fn check(&self) -> Result<()> {
        ensure!(
            !self.0.0.load(Ordering::Acquire),
            "Cancelled by administrator; all attempts retained"
        );
        Ok(())
    }
    fn cancel(&self) {
        self.0.0.store(true, Ordering::Release);
        self.0.1.notify_waiters();
    }
    pub async fn cancelled(&self) {
        loop {
            let notified = self.0.1.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.0.0.load(Ordering::Acquire) {
                return;
            }
            notified.await;
        }
    }
}

tokio::task_local! { static CANCELLATION: Cancellation; }
tokio::task_local! { static MEDIA_PACKAGE: Arc<Mutex<Option<String>>>; }

pub(crate) fn record_media_package(package: &str) {
    let _ = MEDIA_PACKAGE.try_with(|output| {
        *output.lock().expect("media reservation lock") = Some(package.to_owned());
    });
}

pub(crate) fn cancellation() -> Option<Cancellation> {
    CANCELLATION.try_with(Clone::clone).ok()
}

struct Lease {
    cancel: Cancellation,
    source: Option<String>,
    resource: Option<String>,
    scratch: u64,
    media: u64,
    attempt: Option<String>,
    scratch_existing: u64,
    media_package: Arc<Mutex<Option<String>>>,
}

#[derive(Clone, Copy, Default)]
struct Budget {
    scratch: u64,
    media: u64,
}

impl Budget {
    fn add(self, other: Self) -> Result<Self> {
        Ok(Self {
            scratch: self
                .scratch
                .checked_add(other.scratch)
                .context("reservation overflow")?,
            media: self
                .media
                .checked_add(other.media)
                .context("reservation overflow")?,
        })
    }
}

#[derive(Clone, Copy)]
struct Volume {
    device: u64,
    free: u64,
}

fn allocate(
    request: Budget,
    outstanding: Budget,
    scratch: Volume,
    used: u64,
    quota: u64,
    media: Option<Volume>,
) -> Result<()> {
    let demand = request.add(outstanding)?;
    let headroom = 1024 * 1024 * 1024;
    if request.scratch > 0 {
        ensure!(
            used.checked_add(demand.scratch)
                .is_some_and(|bytes| bytes <= quota),
            "scratch quota would be exceeded; retained attempts need operator review"
        );
    }
    if let Some(media) = media.filter(|volume| volume.device == scratch.device) {
        if request.scratch > 0 || request.media > 0 {
            let required = demand
                .scratch
                .checked_add(demand.media)
                .and_then(|bytes| bytes.checked_add(headroom))
                .context("reservation overflow")?;
            ensure!(
                scratch.free.min(media.free) >= required,
                "shared filesystem capacity reserved"
            );
        }
    } else {
        if request.scratch > 0 {
            ensure!(
                scratch.free
                    >= demand
                        .scratch
                        .checked_add(headroom)
                        .context("reservation overflow")?,
                "scratch capacity reserved"
            );
        }
        if request.media > 0 {
            ensure!(
                media.context("Configure the media root")?.free
                    >= demand
                        .media
                        .checked_add(headroom)
                        .context("reservation overflow")?,
                "media capacity reserved"
            );
        }
    }
    Ok(())
}

pub async fn recover(pool: &SqlitePool) -> Result<()> {
    // Only startup under the lifetime owner locks may finalize drained durable results.
    queue::restore_results(pool).await?;
    super::recover(pool).await?;
    super::imports::recover(pool).await?;
    super::conversions::recover(pool).await?;
    super::scans::recover(pool).await?;
    super::comparisons::recover(pool).await?;
    queue::recover(pool).await
}

/// One owner, one global cap, and reservations held until every job-owned task has drained.
pub struct Coordinator {
    pool: SqlitePool,
    imports: Option<Arc<super::imports::ImportConfig>>,
    media: Option<Arc<PreviewRoot>>,
    limits: super::conversions::Limits,
    active: HashMap<i64, Lease>,
    tasks: JoinSet<(i64, Result<bool>)>,
    completed: Vec<(i64, Result<bool>)>,
    maintenance: Instant,
    maintenance_due: bool,
    maintenance_job: Option<tokio::task::JoinHandle<Result<bool>>>,
}

impl Coordinator {
    pub fn new(
        pool: SqlitePool,
        imports: Option<Arc<super::imports::ImportConfig>>,
        media: Option<Arc<PreviewRoot>>,
        limits: super::conversions::Limits,
    ) -> Self {
        Self {
            pool,
            imports,
            media,
            limits,
            active: HashMap::new(),
            tasks: JoinSet::new(),
            completed: Vec::new(),
            maintenance: Instant::now(),
            maintenance_due: true,
            maintenance_job: None,
        }
    }

    pub fn is_active(&self) -> bool {
        !self.active.is_empty() || !self.completed.is_empty() || self.maintenance_job.is_some()
    }

    fn start_maintenance(&mut self) -> Result<()> {
        if self.maintenance_due && self.active.is_empty() && self.maintenance_job.is_none() {
            if let Some(media) = &self.media {
                let pool = self.pool.clone();
                let media = media.clone();
                let scratch = self
                    .imports
                    .as_ref()
                    .map(|config| config.output_root())
                    .transpose()?;
                self.maintenance_job = Some(tokio::spawn(async move {
                    super::trash::tick_quiescent(
                        &pool,
                        &media,
                        scratch.as_ref(),
                        wetube_backend::now(),
                    )
                    .await
                }));
            }
            self.maintenance = Instant::now();
            self.maintenance_due = false;
        }
        Ok(())
    }

    pub async fn tick(&mut self, starts: bool) -> Result<()> {
        if self
            .maintenance_job
            .as_ref()
            .is_some_and(|job| job.is_finished())
        {
            self.maintenance_job.take().unwrap().await??;
        }
        super::heartbeat(&self.pool).await?;
        sqlx::query("DELETE FROM playback_leases WHERE expires_at<=?")
            .bind(wetube_backend::now())
            .execute(&self.pool)
            .await?;
        for (key, lease) in &self.active {
            if queue::cancellation_requested(&self.pool, *key).await? {
                lease.cancel.cancel();
            }
        }
        while let Some(result) = self.tasks.try_join_next() {
            // Never release an unknown panicked task's reservation or acknowledge cancellation early.
            self.completed.push(result?);
        }
        while let Some((key, result)) = self.completed.last() {
            let key = *key;
            if let Err(error) = result {
                tracing::error!(job_key=key, %error, "processing stage failed");
                let detail = format!("{error:#}").chars().take(4096).collect::<String>();
                let row = sqlx::query("SELECT kind,job_id FROM processing_queue WHERE key=?")
                    .bind(key)
                    .fetch_one(&self.pool)
                    .await?;
                let table = queue::table(row.try_get::<&str, _>("kind")?)?;
                sqlx::query(&format!("UPDATE {table} SET error=? WHERE id=? AND state NOT IN ('published','succeeded','needs_review')"))
                    .bind(detail).bind(row.try_get::<String,_>("job_id")?).execute(&self.pool).await?;
                queue::defer_finish(
                    &self.pool,
                    row.try_get::<&str, _>("kind")?,
                    row.try_get::<&str, _>("job_id")?,
                    "failed",
                )
                .await?;
            }
            self.active.remove(&key);
            queue::finish(&self.pool, key).await?;
            self.completed.pop();
        }
        wetube_backend::metadata::expire(&self.pool, wetube_backend::now()).await?;
        self.maintenance_due |= self.maintenance.elapsed() >= Duration::from_secs(60);
        wetube_backend::simple_imports::advance(&self.pool).await?;
        let settings = queue::snapshot(&self.pool).await?;
        let mut revision = settings.revision;
        if !starts || settings.paused {
            self.start_maintenance()?;
            return Ok(());
        }
        let candidates = sqlx::query(
            "SELECT * FROM processing_details WHERE state='waiting' ORDER BY position,key",
        )
        .fetch_all(&self.pool)
        .await?;
        for candidate in candidates {
            if self.active.len() >= usize::from(settings.maximum) {
                break;
            }
            let key = candidate.try_get("key")?;
            let kind: String = candidate.try_get("kind")?;
            let source: Option<String> = candidate.try_get("source")?;
            let resource: Option<String> = candidate.try_get("resource")?;
            let mut reason = None;
            if kind != "diagnostic" && self.imports.is_none() {
                reason = Some("Configure incoming and scratch roots");
            } else if matches!(kind.as_str(), "conversion" | "publication") && self.media.is_none()
            {
                reason = Some("Configure the media root");
            } else if self.active.values().any(|lease| {
                source
                    .as_ref()
                    .is_some_and(|source| lease.source.as_ref() == Some(source))
                    || resource
                        .as_ref()
                        .is_some_and(|resource| lease.resource.as_ref() == Some(resource))
            }) {
                reason = Some("Waiting for retained input or attempt in use");
            }
            let id: String = candidate.try_get("job_id")?;
            let (budget, scratch_existing) = if reason.is_none() {
                let reservation = self
                    .reservation(&kind, &id, source.as_deref(), resource.as_deref())
                    .await;
                let (budget, existing) = match reservation {
                    Ok(budget) => budget,
                    Err(error) => {
                        let Some(claim) =
                            queue::claim_at_revision(&self.pool, key, Some(revision)).await?
                        else {
                            break;
                        };
                        sqlx::query(&format!(
                            "UPDATE {} SET error=? WHERE id=?",
                            queue::table(&kind)?
                        ))
                        .bind(format!("{error:#}").chars().take(4096).collect::<String>())
                        .bind(claim.row.try_get::<String, _>("id")?)
                        .execute(&self.pool)
                        .await?;
                        queue::defer_finish(&self.pool, &kind, &id, "failed").await?;
                        queue::finish(&self.pool, key).await?;
                        revision += 2;
                        continue;
                    }
                };
                match self.require_capacity(budget) {
                    Ok(()) => (budget, existing),
                    Err(_) => {
                        reason = Some("Waiting for storage capacity; review retained attempts");
                        (Budget::default(), 0)
                    }
                }
            } else {
                (Budget::default(), 0)
            };
            if let Some(reason) = reason {
                let changed = sqlx::query("UPDATE processing_queue SET reason=? WHERE key=? AND state='waiting' AND reason!=?")
                    .bind(reason).bind(key).bind(reason).execute(&self.pool).await?.rows_affected();
                if changed > 0 {
                    revision += 1;
                }
                continue;
            }
            let Some(claim) = queue::claim_at_revision(&self.pool, key, Some(revision)).await?
            else {
                break;
            };
            revision += 1;
            let cancel = Cancellation::default();
            let media_package = Arc::new(Mutex::new(None));
            let attempt = if kind == "diagnostic" {
                None
            } else {
                claim.row.try_get("attempt_id")?
            };
            self.active.insert(
                key,
                Lease {
                    cancel: cancel.clone(),
                    source,
                    resource,
                    scratch: budget.scratch,
                    media: budget.media,
                    attempt,
                    scratch_existing,
                    media_package: media_package.clone(),
                },
            );
            let pool = self.pool.clone();
            let config = self.imports.clone();
            let media = self.media.clone();
            let limits = self.limits.clone();
            self.tasks.spawn(CANCELLATION.scope(
                cancel,
                MEDIA_PACKAGE.scope(media_package, async move {
                    let result = match claim.kind.as_str() {
                        "diagnostic" => super::run_diagnostic(&pool, claim.row).await,
                        "scan" => super::scans::run(&pool, config.unwrap(), claim.row).await,
                        "probe" => super::imports::run(&pool, config.unwrap(), claim.row).await,
                        "comparison" => {
                            super::comparisons::run_job(&pool, config.unwrap(), &limits, claim.row)
                                .await
                        }
                        "conversion" | "publication" => {
                            super::conversions::run(
                                &pool,
                                config.unwrap(),
                                &media.unwrap(),
                                &limits,
                                claim.row,
                            )
                            .await
                        }
                        _ => unreachable!(),
                    };
                    (key, result)
                }),
            ));
        }
        self.start_maintenance()?;
        Ok(())
    }

    async fn reservation(
        &self,
        kind: &str,
        id: &str,
        source: Option<&str>,
        resource: Option<&str>,
    ) -> Result<(Budget, u64)> {
        let mib = 1024 * 1024;
        let (scratch, media) = match kind {
            "diagnostic" => (0, 0),
            "scan" | "probe" if kind == "scan" || resource.is_none() => (
                self.imports
                    .as_ref()
                    .unwrap()
                    .source_size(
                        source.context("missing source")?,
                        &sqlx::query_scalar::<_, String>(&format!(
                            "SELECT source_identity FROM {} WHERE id=?",
                            queue::table(kind)?
                        ))
                        .bind(id)
                        .fetch_one(&self.pool)
                        .await?,
                    )?
                    .checked_add(4 * mib)
                    .ok_or_else(|| anyhow::anyhow!("reservation overflow"))?,
                0,
            ),
            "probe" => (4 * mib, 0),
            "comparison" => (400 * mib, 0),
            "conversion" => (self.limits.max_output_bytes * 2 + 32 * mib, 0),
            "publication" => {
                let row = sqlx::query(
                    "SELECT attempt_id,report,movie_sha256 FROM conversion_jobs WHERE id=?",
                )
                .bind(id)
                .fetch_one(&self.pool)
                .await?;
                let attempt: String = row.try_get("attempt_id")?;
                let report: serde_json::Value =
                    serde_json::from_str(&row.try_get::<String, _>("report")?)?;
                ensure!(
                    report["status"] == "verified_experimental_output"
                        && report["attempt_id"].as_str() == Some(&attempt)
                        && report["movie_sha256"].as_str()
                            == Some(row.try_get::<&str, _>("movie_sha256")?),
                    "publication requires the successful durable conversion report/hash"
                );
                let root = self.imports.as_ref().unwrap().output_root()?;
                let mut bytes = 16 * mib;
                for (name, identity) in [
                    ("movie.mp4", "movie_identity"),
                    ("original-tracks.mkv", "original_tracks_identity"),
                ] {
                    if name == "original-tracks.mkv"
                        && report["validation"]["original_tracks"].is_null()
                    {
                        continue;
                    }
                    let metadata = root.open_asset(&attempt, name)?.metadata()?;
                    ensure!(
                        metadata.len() > 0
                            && metadata.len() <= self.limits.max_output_bytes
                            && report["validation"][identity].as_str()
                                == Some(wetube_backend::incoming::identity(&metadata).as_str()),
                        "validated publication output changed or exceeds the configured bound"
                    );
                    bytes = bytes
                        .checked_add(metadata.len())
                        .context("publication size overflow")?;
                }
                (0, bytes)
            }
            _ => unreachable!(),
        };
        let existing = if kind == "probe"
            && let Some(resource) = resource
        {
            self.imports
                .as_ref()
                .unwrap()
                .output_root()?
                .output_bytes(resource)?
        } else {
            0
        };
        Ok((Budget { scratch, media }, existing))
    }

    fn require_capacity(&self, budget: Budget) -> Result<()> {
        if budget.scratch == 0 && budget.media == 0 {
            return Ok(());
        }
        let config = self.imports.as_ref().unwrap();
        let scratch_root = config.output_root()?;
        let mut outstanding = Budget::default();
        for lease in self.active.values() {
            let consumed = if lease.scratch > 0
                && let Some(attempt) = &lease.attempt
            {
                scratch_root
                    .output_bytes(attempt)?
                    .saturating_sub(lease.scratch_existing)
            } else {
                0
            };
            let package = lease
                .media_package
                .lock()
                .expect("media reservation lock")
                .clone();
            let media_consumed = if lease.media > 0
                && let Some(package) = package
            {
                self.media.as_ref().unwrap().output_bytes(&package)?
            } else {
                0
            };
            outstanding = outstanding.add(Budget {
                scratch: lease.scratch.saturating_sub(consumed),
                media: lease.media.saturating_sub(media_consumed),
            })?;
        }
        let (device, free, used, quota) = config.storage()?;
        let media = self
            .media
            .as_ref()
            .map(|root| {
                let (device, free) = root.storage()?;
                Ok::<_, anyhow::Error>(Volume { device, free })
            })
            .transpose()?;
        allocate(
            budget,
            outstanding,
            Volume { device, free },
            used,
            quota,
            media,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs::{File, OpenOptions},
        io::{Seek, Write},
        os::unix::fs::MetadataExt,
        process::Stdio,
    };
    use tokio::process::Command;

    fn source_metadata(metadata: &std::fs::Metadata) -> String {
        format!(
            "dev={} ino={} bytes={} mtime={}:{} ctime={}:{}",
            metadata.dev(),
            metadata.ino(),
            metadata.len(),
            metadata.mtime(),
            metadata.mtime_nsec(),
            metadata.ctime(),
            metadata.ctime_nsec()
        )
    }

    async fn source_diagnostics(
        pool: &SqlitePool,
        directory: &tempfile::TempDir,
    ) -> Result<String> {
        let rows = sqlx::query("SELECT * FROM fixture_source_snapshots ORDER BY name")
            .fetch_all(pool)
            .await?;
        let mut detail = format!("jobs={:?}", wetube_backend::scans::list(pool).await?);
        for row in rows {
            let name: String = row.try_get("name")?;
            let current = std::fs::symlink_metadata(directory.path().join("incoming").join(&name));
            detail.push_str(&format!(
                "\n{name}: confirmed={} before={} current={:?}",
                row.try_get::<String, _>("identity")?,
                row.try_get::<String, _>("metadata")?,
                current.as_ref().map(source_metadata)
            ));
        }
        Ok(detail)
    }

    type FixtureSources = Vec<(String, String, String)>;

    async fn stable_fixture_sources(incoming: &std::path::Path) -> Result<FixtureSources> {
        let root = wetube_backend::incoming::IncomingRoot::open(incoming)?;
        let snapshot = || -> Result<(FixtureSources, bool)> {
            let mut sources = Vec::new();
            let mut coherent = true;
            for name in ["one.iso", "two.iso"] {
                let (file, entry) = root.open_file(name)?;
                let metadata = file.metadata()?;
                coherent &= entry.identity == wetube_backend::incoming::identity(&metadata);
                sources.push((name.to_owned(), entry.identity, source_metadata(&metadata)));
            }
            Ok((sources, coherent))
        };
        let started = Instant::now();
        let mut quiet_since = started;
        let (mut previous, _) = snapshot()?;
        loop {
            ensure!(
                started.elapsed() < Duration::from_secs(10),
                "fixture source metadata did not settle before confirmation: {previous:?}"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
            let (current, coherent) = snapshot()?;
            if !coherent || current != previous {
                quiet_since = Instant::now();
                previous = current;
            } else if quiet_since.elapsed() >= Duration::from_secs(2) {
                return Ok(previous);
            }
        }
    }

    async fn coordinator_fixture(
        shared: bool,
        quota: u64,
    ) -> Result<(
        tempfile::TempDir,
        SqlitePool,
        Arc<super::super::imports::ImportConfig>,
    )> {
        let directory = tempfile::tempdir()?;
        let state = directory.path().join("state");
        let incoming = directory.path().join("incoming");
        let scratch = directory.path().join("scratch");
        std::fs::create_dir(&state)?;
        std::fs::create_dir(&incoming)?;
        wetube_backend::db::initialize(&state).await?;
        let pool = wetube_backend::db::connect(&state).await?;
        sqlx::query("INSERT INTO users VALUES('viewer','viewer','hash','viewer')")
            .execute(&pool)
            .await?;
        sqlx::query("CREATE TABLE fixture_source_snapshots(name TEXT PRIMARY KEY,identity TEXT,metadata TEXT)").execute(&pool).await?;
        let config = Arc::new(super::super::imports::ImportConfig::open(
            &incoming,
            &scratch,
            1024,
            quota,
            Duration::from_secs(5),
        )?);
        for name in ["one.iso", "two.iso"] {
            let mut writer = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(incoming.join(name))?;
            writer.write_all(b"owned test input")?;
            writer.sync_all()?;
            drop(writer);
        }
        File::open(&incoming)?.sync_all()?;
        // Tiny C-backed copies can expose delayed metadata after writer close.
        // Require a quiet window before confirmation, never a later rebaseline.
        let sources = stable_fixture_sources(&incoming).await?;
        for (id, name) in [
            ("one", "one.iso"),
            ("two", if shared { "one.iso" } else { "two.iso" }),
        ] {
            let (file, entry) =
                wetube_backend::incoming::IncomingRoot::open(&incoming)?.open_file(name)?;
            let (_, identity, metadata) = sources
                .iter()
                .find(|source| source.0 == name)
                .context("missing fixture source")?;
            let observed = file.metadata()?;
            ensure!(
                &entry.identity == identity
                    && wetube_backend::incoming::identity(&observed) == *identity,
                "fixture source changed after the pre-confirmation quiet window: {name}: \
                 expected={identity} observed={} before={metadata} current={}",
                entry.identity,
                source_metadata(&observed)
            );
            sqlx::query("INSERT OR IGNORE INTO fixture_source_snapshots VALUES(?,?,?)")
                .bind(name)
                .bind(identity)
                .bind(metadata)
                .execute(&pool)
                .await?;
            sqlx::query("INSERT INTO scan_jobs(id,requested_by,name,source_identity,state,created_at,simple_state,simple_requested_by) VALUES(?,'viewer',?,?,'queued',1,CASE WHEN ? THEN NULL ELSE 'scan' END,'viewer')")
                .bind(id).bind(name).bind(identity).bind(shared && id=="two").execute(&pool).await?;
        }
        let snapshot = queue::snapshot(&pool).await?;
        queue::settings(
            &pool,
            &wetube_core::QueueSettings {
                revision: snapshot.revision,
                paused: false,
                maximum: 2,
            },
        )
        .await?;
        Ok((directory, pool, config))
    }

    #[tokio::test]
    async fn processing_queue_paused_maintenance_expires_leases_without_starving_purge()
    -> Result<()> {
        let (directory, pool, config) = coordinator_fixture(false, 16 * 1024 * 1024).await?;
        let media_path = directory.path().join("media");
        std::fs::create_dir(&media_path)?;
        let media = Arc::new(PreviewRoot::open(&media_path)?);
        sqlx::query("INSERT INTO movies VALUES('expired','Expired',NULL,'','trashed'),('protected','Protected',NULL,'','trashed')").execute(&pool).await?;
        sqlx::query("INSERT INTO movie_trash(movie_id,deleted_at,purge_after,provenance_complete) VALUES('expired',1,1,1),('protected',1,1,1)").execute(&pool).await?;
        sqlx::query("INSERT INTO preview_media VALUES('expired','expired-package',?,1,1.0,'already-removed',1),('protected','protected-package',?,1,1.0,'already-removed',1)")
            .bind("a".repeat(64)).bind("b".repeat(64)).execute(&pool).await?;
        sqlx::query(
            "INSERT INTO sessions VALUES('isolated-session','viewer','test-only',9999999999)",
        )
        .execute(&pool)
        .await?;
        sqlx::query("INSERT INTO playback_lease_bindings VALUES('expired-lease','expired','isolated-session'),('protected-lease','protected','isolated-session')").execute(&pool).await?;
        sqlx::query("INSERT INTO playback_leases VALUES('expired-lease','expired','isolated-session',1),('protected-lease','protected','isolated-session',9999999999)").execute(&pool).await?;
        let snapshot = queue::snapshot(&pool).await?;
        queue::settings(
            &pool,
            &wetube_core::QueueSettings {
                revision: snapshot.revision,
                paused: true,
                maximum: 1,
            },
        )
        .await?;
        let mut coordinator = Coordinator::new(pool.clone(), Some(config), Some(media), limits());
        coordinator.tick(true).await?;
        tokio::time::timeout(Duration::from_secs(5), async {
            while coordinator.is_active() {
                tokio::time::sleep(Duration::from_millis(10)).await;
                coordinator.tick(true).await?;
            }
            anyhow::Ok(())
        })
        .await??;
        assert!(!coordinator.is_active());
        assert_eq!(
            sqlx::query_scalar::<_, String>("SELECT id FROM movies")
                .fetch_one(&pool)
                .await?,
            "protected"
        );
        assert_eq!(
            sqlx::query_scalar::<_, String>("SELECT id FROM playback_leases")
                .fetch_one(&pool)
                .await?,
            "protected-lease"
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM processing_queue WHERE state='waiting'"
            )
            .fetch_one(&pool)
            .await?,
            2
        );
        assert_eq!(
            sqlx::query_scalar::<_, String>("SELECT movie_id FROM movie_purge_history")
                .fetch_one(&pool)
                .await?,
            "expired"
        );
        pool.close().await;
        Ok(())
    }

    fn limits() -> super::super::conversions::Limits {
        super::super::conversions::Limits {
            timeout: Duration::from_secs(5),
            threads: 2,
            max_output_bytes: 1024 * 1024,
        }
    }

    #[test]
    fn processing_queue_allocation_separates_devices_and_counts_only_remaining_bytes() -> Result<()>
    {
        let gib = 1024 * 1024 * 1024;
        let scratch = Volume {
            device: 1,
            free: 12 * gib,
        };
        let media = Volume { device: 2, free: 0 };
        let scratch_job = Budget {
            scratch: 4 * gib,
            media: 0,
        };
        allocate(
            scratch_job,
            Budget::default(),
            scratch,
            0,
            32 * gib,
            Some(media),
        )?;
        let publication = Budget {
            scratch: 0,
            media: 4 * gib + 16 * 1024 * 1024,
        };
        allocate(scratch_job, publication, scratch, 0, 32 * gib, Some(media))?;
        allocate(
            publication,
            Budget::default(),
            Volume { free: 0, ..scratch },
            32 * gib,
            32 * gib,
            Some(Volume {
                free: 12 * gib,
                ..media
            }),
        )?;
        assert!(
            allocate(
                scratch_job,
                Budget {
                    scratch: 4 * gib,
                    media: 0
                },
                scratch,
                26 * gib,
                32 * gib,
                Some(media)
            )
            .is_err()
        );
        let shared = Volume {
            device: 1,
            free: 8 * gib,
        };
        assert!(allocate(scratch_job, publication, shared, 0, 32 * gib, Some(shared)).is_err());
        let shared_room = Volume {
            free: 10 * gib,
            ..shared
        };
        allocate(
            scratch_job,
            publication,
            shared_room,
            0,
            32 * gib,
            Some(shared_room),
        )?;
        allocate(
            Budget {
                scratch: 2 * gib,
                media: 0,
            },
            Budget {
                scratch: gib,
                media: 0,
            },
            scratch,
            5 * gib,
            8 * gib,
            Some(media),
        )?;
        assert!(
            allocate(
                Budget {
                    scratch: 2 * gib,
                    media: 0
                },
                Budget {
                    scratch: 4 * gib,
                    media: 0
                },
                scratch,
                5 * gib,
                8 * gib,
                Some(media)
            )
            .is_err()
        );
        Ok(())
    }

    #[tokio::test]
    async fn processing_queue_reservations_bind_pinned_roots_and_credit_owned_output_growth()
    -> Result<()> {
        let mib = 1024 * 1024;
        let (directory, pool, config) = coordinator_fixture(false, 8 * mib + 100).await?;
        let snapshot = queue::snapshot(&pool).await?;
        queue::settings(
            &pool,
            &wetube_core::QueueSettings {
                revision: snapshot.revision,
                paused: false,
                maximum: 1,
            },
        )
        .await?;
        let mut coordinator = Coordinator::new(pool.clone(), Some(config.clone()), None, limits());
        coordinator.tick(true).await?;
        assert_eq!(
            coordinator.active.len(),
            1,
            "{}",
            source_diagnostics(&pool, &directory).await?
        );
        let lease = coordinator.active.values().next().unwrap();
        let attempt = lease.attempt.as_ref().unwrap();
        let output = config.scratch_path().join(attempt);
        std::fs::create_dir(&output)?;
        std::fs::write(output.join("input.iso"), vec![b'x'; 3 * mib as usize])?;
        let (budget, _) = coordinator
            .reservation("scan", "two", Some("two.iso"), None)
            .await?;
        coordinator.require_capacity(budget)?;
        std::fs::rename(
            directory.path().join("scratch"),
            directory.path().join("pinned-scratch"),
        )?;
        std::fs::create_dir(directory.path().join("scratch"))?;
        coordinator.require_capacity(budget)?;
        std::fs::write(
            config.scratch_path().join("unrelated-retained"),
            vec![b'x'; 200],
        )?;
        assert!(coordinator.require_capacity(budget).is_err());
        while coordinator.is_active() {
            tokio::time::sleep(Duration::from_millis(10)).await;
            coordinator.tick(false).await?;
        }
        pool.close().await;
        Ok(())
    }

    #[tokio::test]
    async fn processing_queue_publication_reserves_actual_identity_bound_outputs() -> Result<()> {
        let (_directory, pool, config) = coordinator_fixture(false, 16 * 1024 * 1024).await?;
        let attempt = uuid::Uuid::new_v4().to_string();
        let output = config.scratch_path().join(&attempt);
        std::fs::create_dir(&output)?;
        std::fs::write(output.join("movie.mp4"), b"validated movie")?;
        std::fs::write(output.join("original-tracks.mkv"), b"validated tracks")?;
        let root = config.output_root()?;
        let movie = root.open_asset(&attempt, "movie.mp4")?.metadata()?;
        let tracks = root
            .open_asset(&attempt, "original-tracks.mkv")?
            .metadata()?;
        let report = serde_json::json!({
            "attempt_id":attempt,"status":"verified_experimental_output","movie_sha256":"a".repeat(64),
            "validation":{"movie_identity":wetube_backend::incoming::identity(&movie),
                "original_tracks":{},"original_tracks_identity":wetube_backend::incoming::identity(&tracks)}
        });
        sqlx::query("INSERT INTO probe_jobs(id,requested_by,name,source_identity,title,angle,state,created_at) VALUES('probe','viewer','missing-original.iso','bound',1,1,'needs_review',1)").execute(&pool).await?;
        sqlx::query("INSERT INTO conversion_jobs(id,probe_job_id,requested_by,state,created_at,video_treatment,attempt_id,report,movie_sha256) VALUES('publication','probe','viewer','needs_review',1,'preserve',?,?,?)")
            .bind(&attempt).bind(report.to_string()).bind("a".repeat(64)).execute(&pool).await?;
        sqlx::query("UPDATE conversion_jobs SET state='queued_publication' WHERE id='publication'")
            .execute(&pool)
            .await?;
        let coordinator = Coordinator::new(pool.clone(), Some(config), None, limits());
        let (budget, _) = coordinator
            .reservation(
                "publication",
                "publication",
                Some("missing-original.iso"),
                Some(&attempt),
            )
            .await?;
        assert_eq!(budget.scratch, 0);
        assert_eq!(budget.media, movie.len() + tracks.len() + 16 * 1024 * 1024);
        std::fs::write(output.join("movie.mp4"), b"replaced output")?;
        assert!(
            coordinator
                .reservation("publication", "publication", None, Some(&attempt))
                .await
                .is_err()
        );
        pool.close().await;
        Ok(())
    }

    #[tokio::test]
    async fn processing_queue_invalid_sources_fail_without_processing_but_capacity_stays_waiting()
    -> Result<()> {
        for change in ["deleted", "renamed", "replaced", "oversized"] {
            let (directory, pool, config) = coordinator_fixture(false, 4 * 1024 * 1024).await?;
            let source = directory.path().join("incoming/one.iso");
            match change {
                "deleted" => std::fs::remove_file(&source)?,
                "renamed" => {
                    std::fs::rename(&source, directory.path().join("incoming/renamed.iso"))?
                }
                "replaced" => {
                    std::fs::remove_file(&source)?;
                    std::fs::write(&source, b"different source")?;
                }
                "oversized" => std::fs::write(&source, vec![b'x'; 1025])?,
                _ => unreachable!(),
            }
            let mut coordinator =
                Coordinator::new(pool.clone(), Some(config.clone()), None, limits());
            coordinator.tick(true).await?;
            assert!(!coordinator.is_active());
            let jobs = wetube_backend::scans::list(&pool).await?;
            let invalid = jobs.iter().find(|job| job.id == "one").unwrap();
            assert_eq!(invalid.state, wetube_core::ScanState::Failed, "{change}");
            let expected = match change {
                "deleted" | "renamed" => "open incoming ISO",
                "replaced" => "source changed since confirmation",
                "oversized" => "per-file byte limit",
                _ => unreachable!(),
            };
            assert!(
                invalid.error.as_deref().unwrap().contains(expected),
                "{change}: {:?}",
                invalid.error
            );
            assert_eq!(std::fs::read_dir(config.scratch_path())?.count(), 0);
            let waiting = queue::snapshot(&pool).await?.jobs;
            assert_eq!(
                waiting.len(),
                1,
                "{change}: {}",
                source_diagnostics(&pool, &directory).await?
            );
            assert_eq!(waiting[0].id, "two");
            assert!(waiting[0].reason.contains("storage capacity"));
            pool.close().await;
        }
        Ok(())
    }

    #[tokio::test]
    async fn processing_queue_due_maintenance_does_not_block_starts_or_simple_advancement()
    -> Result<()> {
        let (directory, pool, config) = coordinator_fixture(false, 16 * 1024 * 1024).await?;
        sqlx::query("UPDATE scan_jobs SET simple_state=NULL")
            .execute(&pool)
            .await?;
        let held = queue::next(&pool, "scan").await?.unwrap();
        let attempt: String = held.row.try_get("attempt_id")?;
        let media_path = directory.path().join("media");
        std::fs::create_dir(&media_path)?;
        let media = Arc::new(PreviewRoot::open(&media_path)?);
        let mut coordinator = Coordinator::new(pool.clone(), Some(config), Some(media), limits());
        coordinator.active.insert(
            held.key,
            Lease {
                cancel: Cancellation::default(),
                source: Some("one.iso".into()),
                resource: None,
                scratch: 4 * 1024 * 1024,
                media: 0,
                attempt: Some(attempt),
                scratch_existing: 0,
                media_package: Arc::new(Mutex::new(None)),
            },
        );
        let (release, hold) = tokio::sync::oneshot::channel();
        let held_pool = pool.clone();
        coordinator.tasks.spawn(async move {
            hold.await.unwrap();
            queue::defer_finish(&held_pool, "scan", "one", "failed")
                .await
                .unwrap();
            (held.key, Ok(true))
        });
        sqlx::query("INSERT INTO users VALUES('admin','admin','hash','admin')")
            .execute(&pool)
            .await?;
        let inventory = serde_json::json!({"titles":[
            {"title":1,"duration_seconds":60.0,"chapter_count":1,"audio_count":1,"subtitle_count":0,
             "audio_languages":[],"subtitle_languages":[],"angles":[1],"main_feature_candidate":true}
        ]});
        sqlx::query("INSERT INTO scan_jobs(id,requested_by,name,source_identity,state,created_at,simple_state,simple_requested_by,attempt_id,sha256,size_bytes,report)
            VALUES('simple','admin','third.iso','bound','needs_review',1,'scan','admin',?,?,15,?)")
            .bind(uuid::Uuid::new_v4().to_string()).bind("a".repeat(64)).bind(inventory.to_string()).execute(&pool).await?;
        coordinator.maintenance = Instant::now() - Duration::from_secs(61);
        coordinator.tick(true).await?;
        assert_eq!(
            coordinator.active.len(),
            2,
            "the second slot must not wait for the held job"
        );
        assert!(
            coordinator.maintenance_job.is_none(),
            "heavy purge is deferred while leases are active"
        );
        let simple: String =
            sqlx::query_scalar("SELECT simple_state FROM scan_jobs WHERE id='simple'")
                .fetch_one(&pool)
                .await?;
        assert_eq!(simple, "inspection");
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM probe_jobs WHERE scan_job_id='simple'"
            )
            .fetch_one(&pool)
            .await?,
            1
        );
        release.send(()).unwrap();
        while coordinator.is_active() {
            tokio::time::sleep(Duration::from_millis(10)).await;
            coordinator.tick(false).await?;
        }
        pool.close().await;
        Ok(())
    }

    #[tokio::test]
    async fn processing_queue_startup_restores_durable_conversion_and_cancel_winners() -> Result<()>
    {
        for cancel in [false, true] {
            let (directory, pool, config) = coordinator_fixture(false, 16 * 1024 * 1024).await?;
            let _state_owner =
                wetube_backend::lock::MaintenanceLock::acquire(&directory.path().join("state"))?;
            let _scratch_owner = config.acquire_scratch_lock()?;
            sqlx::query("INSERT INTO probe_jobs(id,requested_by,name,source_identity,title,angle,state,created_at) VALUES('probe','viewer','one.iso','bound',1,1,'needs_review',1)").execute(&pool).await?;
            sqlx::query("INSERT INTO conversion_jobs(id,probe_job_id,requested_by,state,created_at,video_treatment) VALUES('conversion','probe','viewer','queued',1,'preserve')").execute(&pool).await?;
            let key: i64 =
                sqlx::query_scalar("SELECT key FROM processing_queue WHERE kind='conversion'")
                    .fetch_one(&pool)
                    .await?;
            let claimed = queue::claim(&pool, key).await?.unwrap();
            let attempt: String = claimed.row.try_get("attempt_id")?;
            let output = config.scratch_path().join(&attempt);
            std::fs::create_dir(&output)?;
            std::fs::write(output.join("report.json"), b"retained verified report")?;
            sqlx::query("UPDATE conversion_jobs SET report=?,movie_sha256=? WHERE id='conversion'")
                .bind("{\"status\":\"verified_experimental_output\"}")
                .bind("a".repeat(64))
                .execute(&pool)
                .await?;
            queue::defer_finish(&pool, "conversion", "conversion", "needs_review").await?;
            queue::next(&pool, "scan").await?.unwrap();
            if cancel {
                assert!(queue::cancel(&pool, key).await?);
            }
            pool.close().await;
            let pool = wetube_backend::db::connect(&directory.path().join("state")).await?;
            recover(&pool).await?;
            recover(&pool).await?;
            let row = sqlx::query("SELECT * FROM conversion_jobs WHERE id='conversion'")
                .fetch_one(&pool)
                .await?;
            assert_eq!(
                queue::job_state(&row)?,
                if cancel { "cancelled" } else { "needs_review" }
            );
            assert_eq!(
                row.try_get::<String, _>("report")?,
                "{\"status\":\"verified_experimental_output\"}"
            );
            assert_eq!(row.try_get::<String, _>("movie_sha256")?, "a".repeat(64));
            assert_eq!(row.try_get::<String, _>("attempt_id")?, attempt);
            if !cancel {
                assert_eq!(row.try_get::<String, _>("phase")?, "awaiting_review");
                assert!(row.try_get::<Option<String>, _>("error")?.is_none());
            }
            assert_eq!(
                std::fs::read(output.join("report.json"))?,
                b"retained verified report"
            );
            let interrupted: String =
                sqlx::query_scalar("SELECT state FROM scan_jobs WHERE id='one'")
                    .fetch_one(&pool)
                    .await?;
            assert_eq!(interrupted, "failed");
            pool.close().await;
        }
        Ok(())
    }

    #[tokio::test]
    async fn processing_queue_coordinator_reserves_excludes_shared_sources_and_drains_lower_cap()
    -> Result<()> {
        let (directory, pool, config) = coordinator_fixture(false, 16 * 1024 * 1024).await?;
        let mut coordinator = Coordinator::new(pool.clone(), Some(config), None, limits());
        coordinator.tick(true).await?;
        assert_eq!(
            coordinator.active.len(),
            2,
            "{}",
            source_diagnostics(&pool, &directory).await?
        );
        let snapshot = queue::snapshot(&pool).await?;
        queue::settings(
            &pool,
            &wetube_core::QueueSettings {
                revision: snapshot.revision,
                paused: true,
                maximum: 1,
            },
        )
        .await?;
        assert_eq!(
            coordinator.active.len(),
            2,
            "lowering does not kill existing jobs"
        );
        tokio::time::timeout(Duration::from_secs(5), async {
            while coordinator.is_active() {
                tokio::time::sleep(Duration::from_millis(20)).await;
                coordinator.tick(false).await?;
            }
            anyhow::Ok(())
        })
        .await??;
        assert!(queue::snapshot(&pool).await?.paused);
        assert!(
            wetube_backend::scans::list(&pool)
                .await?
                .iter()
                .all(|job| job.state == wetube_core::ScanState::Failed)
        );
        pool.close().await;
        let (_directory, pool, config) = coordinator_fixture(true, 16 * 1024 * 1024).await?;
        let mut coordinator = Coordinator::new(pool.clone(), Some(config), None, limits());
        coordinator.tick(true).await?;
        assert_eq!(coordinator.active.len(), 1);
        assert!(
            queue::snapshot(&pool)
                .await?
                .jobs
                .iter()
                .any(|job| job.state == "waiting" && job.reason.contains("in use"))
        );
        tokio::time::timeout(Duration::from_secs(5), async {
            while coordinator.is_active() {
                tokio::time::sleep(Duration::from_millis(20)).await;
                coordinator.tick(false).await?;
            }
            anyhow::Ok(())
        })
        .await??;
        coordinator.tick(true).await?;
        assert_eq!(coordinator.active.len(), 1);
        while coordinator.is_active() {
            tokio::time::sleep(Duration::from_millis(20)).await;
            coordinator.tick(false).await?;
        }
        pool.close().await;
        Ok(())
    }

    #[tokio::test]
    async fn processing_queue_manual_cap_never_bypasses_storage_or_recovery_ownership() -> Result<()>
    {
        let (directory, pool, config) = coordinator_fixture(false, 4 * 1024 * 1024).await?;
        let mut coordinator = Coordinator::new(pool.clone(), Some(config), None, limits());
        coordinator.tick(true).await?;
        assert!(!coordinator.is_active());
        assert!(
            queue::snapshot(&pool)
                .await?
                .jobs
                .iter()
                .any(|job| job.reason.contains("storage")),
            "{}",
            source_diagnostics(&pool, &directory).await?
        );
        let state = directory.path().join("state");
        let first = wetube_backend::lock::MaintenanceLock::acquire(&state)?;
        assert!(wetube_backend::lock::MaintenanceLock::acquire(&state).is_err());
        let job = queue::snapshot(&pool).await?.jobs[0].clone();
        let claimed = queue::claim(&pool, job.key).await?.unwrap();
        let attempt: String = claimed.row.try_get("attempt_id")?;
        let retained = directory.path().join("scratch").join(&attempt);
        std::fs::create_dir(&retained)?;
        std::fs::write(retained.join("partial.bin"), b"retained partial")?;
        drop(first);
        let _exclusive = wetube_backend::lock::MaintenanceLock::acquire(&state)?;
        super::super::scans::recover(&pool).await?;
        queue::recover(&pool).await?;
        assert_eq!(
            std::fs::read(retained.join("partial.bin"))?,
            b"retained partial"
        );
        assert_eq!(
            wetube_backend::scans::list(&pool)
                .await?
                .iter()
                .find(|scan| scan.id == job.id)
                .unwrap()
                .attempt_id
                .as_deref(),
            Some(attempt.as_str())
        );
        assert!(queue::claim(&pool, job.key).await?.is_none());
        pool.close().await;
        Ok(())
    }

    #[tokio::test]
    async fn processing_queue_active_cancel_reaps_owned_group_and_keeps_unrelated_helper()
    -> Result<()> {
        let directory = tempfile::tempdir()?;
        let ready = directory.path().join("ready");
        let writes = directory.path().join("writes");
        let unrelated = directory.path().join("unrelated");
        let mut companion = Command::new(std::env::current_exe()?)
            .args([
                "--ignored",
                "--exact",
                "coordinator::tests::processing_queue_unrelated_helper",
            ])
            .env("WETUBE_QUEUE_UNRELATED", &unrelated)
            .stdout(Stdio::null())
            .spawn()?;
        let cancel = Cancellation::default();
        let child_cancel = cancel.clone();
        let child_ready = ready.clone();
        let child_writes = writes.clone();
        let running = tokio::spawn(CANCELLATION.scope(child_cancel, async move {
            super::super::process::run(
                Command::new(std::env::current_exe()?)
                    .args([
                        "--ignored",
                        "--exact",
                        "coordinator::tests::processing_queue_owned_helper",
                    ])
                    .env("WETUBE_QUEUE_READY", child_ready)
                    .env("WETUBE_QUEUE_WRITES", child_writes),
                Duration::from_secs(10),
                64 * 1024,
            )
            .await
        }));
        tokio::time::timeout(Duration::from_secs(5), async {
            while !ready.exists() || !writes.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await?;
        let pids = std::fs::read_to_string(&ready)?
            .split_whitespace()
            .map(str::parse::<u32>)
            .collect::<Result<Vec<_>, _>>()?;
        cancel.cancel();
        let result = tokio::time::timeout(Duration::from_secs(5), running).await??;
        // The runner must report cancellation, not a successful exit.
        assert!(result.err().unwrap().to_string().contains("Cancelled"));
        for pid in pids {
            assert!(
                !std::path::Path::new(&format!("/proc/{pid}")).exists(),
                "owned descendant was not reaped"
            );
        }
        let kept = std::fs::read(&writes)?;
        assert!(!kept.is_empty());
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            std::fs::read(&writes)?,
            kept,
            "no writes may continue after cancellation acknowledgement"
        );
        assert!(companion.wait().await?.success());
        assert_eq!(std::fs::read(&unrelated)?, b"unrelated finished");
        Ok(())
    }

    #[test]
    #[ignore = "isolated owned subprocess fixture"]
    fn processing_queue_owned_helper() {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "coordinator::tests::processing_queue_writing_descendant",
            ])
            .spawn()
            .unwrap();
        let ready = std::env::var_os("WETUBE_QUEUE_READY").unwrap();
        std::fs::write(ready, format!("{} {}", std::process::id(), child.id())).unwrap();
        let _ = child.wait();
    }

    #[test]
    #[ignore = "isolated owned descendant fixture"]
    fn processing_queue_writing_descendant() {
        let path = std::env::var_os("WETUBE_QUEUE_WRITES").unwrap();
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(path)
            .unwrap();
        for _ in 0..500 {
            file.write_all(b"owned\n").unwrap();
            file.sync_all().unwrap();
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    #[ignore = "isolated unrelated subprocess fixture"]
    fn processing_queue_unrelated_helper() {
        std::thread::sleep(Duration::from_millis(500));
        std::fs::write(
            std::env::var_os("WETUBE_QUEUE_UNRELATED").unwrap(),
            b"unrelated finished",
        )
        .unwrap();
    }

    #[tokio::test]
    async fn processing_queue_blocking_hash_observes_cancel_and_drains() -> Result<()> {
        let file = tempfile::NamedTempFile::new()?;
        let size = 16 * 1024 * 1024;
        std::fs::write(file.path(), vec![b'x'; size])?;
        let pool = SqlitePool::connect("sqlite::memory:").await?;
        sqlx::query("CREATE TABLE scan_jobs(id TEXT,progress TEXT)")
            .execute(&pool)
            .await?;
        sqlx::query("INSERT INTO scan_jobs VALUES('hash',NULL)")
            .execute(&pool)
            .await?;
        let cancel = Cancellation::default();
        let recording = CANCELLATION
            .scope(cancel.clone(), async {
                super::super::progress::Recording::start(
                    &pool,
                    "scan_jobs",
                    "hash",
                    wetube_core::MediaWorkflow::Scan,
                    0,
                )
            })
            .await;
        let progress = recording.progress.clone();
        let mut source = File::open(file.path())?;
        let (started, start) = tokio::sync::oneshot::channel();
        let task = tokio::task::spawn_blocking(move || {
            let _ = started.send(());
            let result = super::super::experimental::bounded_hash_progress(
                &mut source,
                size as u64,
                Duration::from_secs(5),
                Some(&progress),
            );
            (result, source.stream_position())
        });
        start.await?;
        tokio::time::sleep(Duration::from_millis(10)).await;
        cancel.cancel();
        let (result, position) = task.await?;
        assert!(result.unwrap_err().to_string().contains("Cancelled"));
        let position = position?;
        assert!(
            position > 0 && position < size as u64,
            "cancellation must be checked while hashing, not only before it starts"
        );
        recording.finish().await;
        assert_eq!(std::fs::metadata(file.path())?.len(), size as u64);
        pool.close().await;
        Ok(())
    }
}
