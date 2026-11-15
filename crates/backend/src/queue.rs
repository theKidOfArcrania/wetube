use anyhow::{Result, ensure};
use sqlx::{Row, SqlitePool, sqlite::SqliteRow};
use wetube_core::{ProcessingJob, ProcessingQueue, QueueOrder, QueueSettings};

pub fn table(kind: &str) -> Result<&'static str> {
    Ok(match kind {
        "diagnostic" => "diagnostic_jobs",
        "scan" => "scan_jobs",
        "probe" => "probe_jobs",
        "comparison" => "comparison_jobs",
        "conversion" | "publication" => "conversion_jobs",
        _ => anyhow::bail!("Unknown processing stage"),
    })
}

pub fn job_state(row: &SqliteRow) -> Result<String> {
    if row.try_get::<bool, _>("cancelled")? {
        Ok("cancelled".into())
    } else {
        Ok(row.try_get("state")?)
    }
}

pub async fn snapshot(pool: &SqlitePool) -> Result<ProcessingQueue> {
    let mut tx = pool.begin().await?;
    let settings = sqlx::query("SELECT * FROM processing_settings WHERE singleton=1")
        .fetch_one(&mut *tx)
        .await?;
    let jobs = sqlx::query("SELECT * FROM processing_details WHERE state IN ('waiting','running','cancelling')
        OR key IN (SELECT key FROM processing_queue WHERE state='cancelled' ORDER BY key DESC LIMIT 20)
        ORDER BY CASE state WHEN 'running' THEN 0 WHEN 'cancelling' THEN 0 WHEN 'waiting' THEN 1 ELSE 2 END,position,key")
        .fetch_all(&mut *tx).await?.into_iter().map(|row| Ok(ProcessingJob {
            key: row.try_get("key")?, kind: row.try_get("kind")?, id: row.try_get("job_id")?,
            name: row.try_get("name")?, state: row.try_get("state")?,
            reason: if settings.try_get::<bool,_>("paused")? && row.try_get::<&str,_>("state")?=="waiting" {
                "Paused; resume new starts to process this job".into()
            } else { row.try_get("reason")? },
        })).collect::<Result<Vec<_>>>()?;
    tx.commit().await?;
    Ok(ProcessingQueue {
        revision: settings.try_get("revision")?,
        paused: settings.try_get("paused")?,
        maximum: settings.try_get("maximum")?,
        jobs,
    })
}

pub async fn settings(pool: &SqlitePool, request: &QueueSettings) -> Result<bool> {
    ensure!(
        (1..=16).contains(&request.maximum),
        "Simultaneous jobs must be between 1 and 16"
    );
    Ok(sqlx::query("UPDATE processing_settings SET paused=?,maximum=?,revision=revision+1 WHERE singleton=1 AND revision=?")
        .bind(request.paused).bind(request.maximum).bind(request.revision).execute(pool).await?.rows_affected()==1)
}

pub async fn reorder(pool: &SqlitePool, request: &QueueOrder) -> Result<bool> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let revision: i64 = sqlx::query_scalar("SELECT revision FROM processing_settings")
        .fetch_one(&mut *tx)
        .await?;
    let mut waiting: Vec<i64> =
        sqlx::query_scalar("SELECT key FROM processing_queue WHERE state='waiting' ORDER BY key")
            .fetch_all(&mut *tx)
            .await?;
    let mut supplied = request.waiting.clone();
    supplied.sort_unstable();
    waiting.sort_unstable();
    if revision != request.revision || supplied != waiting {
        return Ok(false);
    }
    for (position, key) in request.waiting.iter().enumerate() {
        sqlx::query("UPDATE processing_queue SET position=? WHERE key=? AND state='waiting'")
            .bind(position as i64)
            .bind(key)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(true)
}

pub struct Claim {
    pub key: i64,
    pub kind: String,
    pub row: SqliteRow,
}

/// Only the process holding state and scratch MaintenanceLocks may claim or recover work.
pub async fn claim(pool: &SqlitePool, key: i64) -> Result<Option<Claim>> {
    claim_at_revision(pool, key, None).await
}

pub async fn claim_at_revision(
    pool: &SqlitePool,
    key: i64,
    revision: Option<i64>,
) -> Result<Option<Claim>> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let q = sqlx::query("UPDATE processing_queue SET state='running',reason='Processing' WHERE key=? AND state='waiting'
        AND (? IS NULL OR (SELECT revision FROM processing_settings)=?)
        AND (SELECT paused=0 AND (SELECT COUNT(*) FROM processing_queue WHERE state IN ('running','cancelling'))<maximum FROM processing_settings)
        RETURNING kind,job_id").bind(key).bind(revision).bind(revision).fetch_optional(&mut *tx).await?;
    let Some(q) = q else { return Ok(None) };
    let kind: String = q.try_get("kind")?;
    let id: String = q.try_get("job_id")?;
    let attempt = uuid::Uuid::new_v4().to_string();
    let update = match kind.as_str() {
        "diagnostic" => {
            "UPDATE diagnostic_jobs SET state='running',started_at=?,error=NULL WHERE id=? AND state='queued' RETURNING *"
        }
        "scan" => {
            "UPDATE scan_jobs SET state='staging',attempt_id=? WHERE id=? AND state='queued' RETURNING *"
        }
        "probe" => {
            "UPDATE probe_jobs SET state=CASE WHEN scan_job_id IS NULL THEN 'staging' ELSE 'probing' END,attempt_id=CASE WHEN scan_job_id IS NULL THEN ? ELSE attempt_id END WHERE id=? AND state='queued' RETURNING *"
        }
        "comparison" => {
            "UPDATE comparison_jobs SET state='running',attempt_id=? WHERE id=? AND state='queued' RETURNING *"
        }
        "conversion" => {
            "UPDATE conversion_jobs SET state='converting',phase='revalidating_input',attempt_id=? WHERE id=? AND state='queued' RETURNING *"
        }
        "publication" => {
            "UPDATE conversion_jobs SET state='publishing',phase='publishing',attempt_id=COALESCE(attempt_id,?) WHERE id=? AND state='queued_publication' RETURNING *"
        }
        _ => unreachable!(),
    };
    let query = sqlx::query(update);
    let query = if kind == "diagnostic" {
        query.bind(super::now())
    } else {
        query.bind(attempt)
    };
    let row = query.bind(id).fetch_optional(&mut *tx).await?;
    if row.is_none() {
        sqlx::query("UPDATE processing_queue SET state='completed',reason='Stage already completed' WHERE key=?").bind(key).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(row.map(|row| Claim { key, kind, row }))
}

pub async fn next(pool: &SqlitePool, kind: &str) -> Result<Option<Claim>> {
    let key: Option<i64> = sqlx::query_scalar("SELECT key FROM processing_queue WHERE state='waiting' AND kind=? ORDER BY position,key LIMIT 1")
        .bind(kind).fetch_optional(pool).await?;
    match key {
        Some(key) => claim(pool, key).await,
        None => Ok(None),
    }
}

pub async fn cancellation_requested(pool: &SqlitePool, key: i64) -> Result<bool> {
    Ok(
        sqlx::query_scalar("SELECT state='cancelling' FROM processing_queue WHERE key=?")
            .bind(key)
            .fetch_one(pool)
            .await?,
    )
}

/// Keep the job active until the owning runner and its resource lease have drained.
pub async fn defer_finish(pool: &SqlitePool, kind: &str, id: &str, state: &str) -> Result<()> {
    ensure!(
        matches!(state, "failed" | "needs_review" | "succeeded"),
        "Invalid processing result"
    );
    let changed=sqlx::query("UPDATE processing_queue SET finish_state=? WHERE kind=? AND job_id=? AND state IN ('running','cancelling')")
        .bind(state).bind(kind).bind(id).execute(pool).await?.rows_affected();
    ensure!(
        changed == 1,
        "Processing ownership changed before finalization"
    );
    Ok(())
}

pub async fn cancel(pool: &SqlitePool, key: i64) -> Result<bool> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let q = sqlx::query("SELECT * FROM processing_queue WHERE key=? AND state IN ('waiting','running','cancelling')")
        .bind(key).fetch_optional(&mut *tx).await?;
    let Some(q) = q else { return Ok(false) };
    let kind: String = q.try_get("kind")?;
    let id: String = q.try_get("job_id")?;
    let table = table(&kind)?;
    let current: String = sqlx::query_scalar(&format!("SELECT state FROM {table} WHERE id=?"))
        .bind(&id)
        .fetch_one(&mut *tx)
        .await?;
    if !matches!(
        current.as_str(),
        "queued"
            | "queued_publication"
            | "running"
            | "staging"
            | "scanning"
            | "probing"
            | "converting"
            | "publishing"
    ) {
        return Ok(false);
    }
    let waiting = q.try_get::<String, _>("state")? == "waiting";
    sqlx::query("UPDATE processing_queue SET state=?,reason=? WHERE key=?")
        .bind(if waiting { "cancelled" } else { "cancelling" })
        .bind(if waiting {
            "Cancelled; retained outputs unchanged"
        } else {
            "Cancelling; waiting for owned work to stop"
        })
        .bind(key)
        .execute(&mut *tx)
        .await?;
    if waiting {
        sqlx::query(&format!("UPDATE {table} SET state='failed',cancelled=1,error='Cancelled by administrator; all attempts retained' WHERE id=?")).bind(&id).execute(&mut *tx).await?;
    }
    sqlx::query("UPDATE scan_jobs SET simple_state='manual',simple_message='Cancelled. Retained results are available in Advanced; no automatic continuation.'
        WHERE simple_state IS NOT NULL AND (id=? OR simple_probe_id=? OR simple_conversion_id=?)")
        .bind(if kind=="scan" { &id } else { "" }).bind(if kind=="probe" { &id } else { "" })
        .bind(if matches!(kind.as_str(),"conversion"|"publication") { &id } else { "" }).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(true)
}

/// Call after processes, blocking tasks, progress writers and resource leases have drained.
pub async fn finish(pool: &SqlitePool, key: i64) -> Result<()> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let q = sqlx::query("SELECT * FROM processing_queue WHERE key=?")
        .bind(key)
        .fetch_one(&mut *tx)
        .await?;
    if q.try_get::<String, _>("state")? == "cancelling" {
        let kind: String = q.try_get("kind")?;
        sqlx::query(&format!("UPDATE {} SET state='failed',cancelled=1,error='Cancelled by administrator; all attempts retained' WHERE id=? AND state!='published'",table(&kind)?))
            .bind(q.try_get::<String,_>("job_id")?).execute(&mut *tx).await?;
        sqlx::query("UPDATE processing_queue SET state='cancelled',reason='Cancelled; retained outputs unchanged' WHERE key=?").bind(key).execute(&mut *tx).await?;
    } else {
        if q.try_get::<&str, _>("state")? == "running"
            && let Some(state) = q.try_get::<Option<String>, _>("finish_state")?
        {
            let kind: String = q.try_get("kind")?;
            sqlx::query(&format!(
                "UPDATE {} SET state=? WHERE id=? AND state!='published'",
                table(&kind)?
            ))
            .bind(state)
            .bind(q.try_get::<String, _>("job_id")?)
            .execute(&mut *tx)
            .await?;
        }
        sqlx::query("UPDATE processing_queue SET state='completed',reason='Stage finished' WHERE key=? AND state='running'").bind(key).execute(&mut *tx).await?;
    }
    if q.try_get::<&str, _>("kind")? == "diagnostic" {
        sqlx::query("UPDATE diagnostic_jobs SET finished_at=? WHERE id=? AND state IN ('failed','succeeded')")
            .bind(super::now()).bind(q.try_get::<String,_>("job_id")?).execute(&mut *tx).await?;
    } else if q.try_get::<&str, _>("kind")? == "conversion" {
        sqlx::query("UPDATE conversion_jobs SET phase='awaiting_review' WHERE id=? AND state='needs_review'")
            .bind(q.try_get::<String,_>("job_id")?).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Startup only, after exclusive worker ownership proves the old runners drained.
pub async fn restore_results(pool: &SqlitePool) -> Result<()> {
    let keys: Vec<i64> = sqlx::query_scalar(
        "SELECT key FROM processing_queue WHERE state IN ('running','cancelling')
         AND (finish_state IS NOT NULL OR state='cancelling'
              OR (kind='publication' AND EXISTS(SELECT 1 FROM conversion_jobs WHERE id=job_id AND state='published')))
         ORDER BY key",
    ).fetch_all(pool).await?;
    for key in keys {
        finish(pool, key).await?;
    }
    Ok(())
}

pub async fn recover(pool: &SqlitePool) -> Result<()> {
    restore_results(pool).await?;
    sqlx::query("UPDATE processing_queue SET state='waiting',reason='Waiting for worker' WHERE kind='diagnostic' AND state='running' AND EXISTS(SELECT 1 FROM diagnostic_jobs WHERE id=job_id AND state='queued')")
        .execute(pool).await?;
    let keys: Vec<i64> = sqlx::query_scalar(
        "SELECT key FROM processing_queue WHERE state IN ('running','cancelling') ORDER BY key",
    )
    .fetch_all(pool)
    .await?;
    for key in keys {
        finish(pool, key).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn fixture() -> Result<(tempfile::TempDir, SqlitePool)> {
        let directory = tempfile::tempdir()?;
        crate::db::initialize(directory.path()).await?;
        let pool = crate::db::connect(directory.path()).await?;
        sqlx::query("INSERT INTO users VALUES('admin','admin','hash','admin')")
            .execute(&pool)
            .await?;
        sqlx::query("INSERT INTO diagnostic_jobs(id,requested_by,state,created_at) VALUES('diagnostic','admin','queued',1)").execute(&pool).await?;
        sqlx::query("INSERT INTO scan_jobs(id,requested_by,name,source_identity,state,created_at,simple_state,simple_requested_by) VALUES('scan','admin','one.iso','bound','queued',1,'scan','admin')").execute(&pool).await?;
        sqlx::query("INSERT INTO probe_jobs(id,requested_by,name,source_identity,title,angle,state,created_at,attempt_id,report) VALUES('probe','admin','two.iso','bound',1,1,'queued',1,'retained','{}')").execute(&pool).await?;
        sqlx::query("INSERT INTO comparison_jobs(id,probe_job_id,requested_by,state,created_at,probe_report,title,angle) VALUES('comparison','probe','admin','queued',1,'{}',1,1)").execute(&pool).await?;
        sqlx::query("INSERT INTO conversion_jobs(id,probe_job_id,requested_by,state,created_at,video_treatment) VALUES('conversion','probe','admin','queued',1,'preserve')").execute(&pool).await?;
        sqlx::query("INSERT INTO probe_jobs(id,requested_by,name,source_identity,title,angle,state,created_at,attempt_id,report) VALUES('publication-probe','admin','three.iso','bound',1,1,'needs_review',1,'retained','{}')").execute(&pool).await?;
        sqlx::query("INSERT INTO conversion_jobs(id,probe_job_id,requested_by,state,created_at,video_treatment,attempt_id) VALUES('publication','publication-probe','admin','failed',1,'preserve','retained')").execute(&pool).await?;
        sqlx::query("UPDATE conversion_jobs SET state='queued_publication' WHERE id='publication'")
            .execute(&pool)
            .await?;
        Ok((directory, pool))
    }

    #[tokio::test]
    async fn processing_queue_persists_whole_order_pause_resume_and_stale_conflicts() -> Result<()>
    {
        let (directory, pool) = fixture().await?;
        let initial = snapshot(&pool).await?;
        assert_eq!(
            initial
                .jobs
                .iter()
                .map(|job| job.kind.as_str())
                .collect::<Vec<_>>(),
            [
                "diagnostic",
                "scan",
                "probe",
                "comparison",
                "conversion",
                "publication"
            ]
        );
        assert!(
            settings(
                &pool,
                &QueueSettings {
                    revision: initial.revision,
                    paused: true,
                    maximum: 2
                }
            )
            .await?
        );
        assert!(claim(&pool, initial.jobs[0].key).await?.is_none());
        let paused = snapshot(&pool).await?;
        let mut order = paused.jobs.iter().map(|job| job.key).collect::<Vec<_>>();
        order.reverse();
        assert!(
            reorder(
                &pool,
                &QueueOrder {
                    revision: paused.revision,
                    waiting: order.clone()
                }
            )
            .await?
        );
        assert!(
            !reorder(
                &pool,
                &QueueOrder {
                    revision: paused.revision,
                    waiting: order.clone()
                }
            )
            .await?
        );
        let updated = snapshot(&pool).await?;
        assert_eq!(
            updated.jobs.iter().map(|job| job.key).collect::<Vec<_>>(),
            order
        );
        // A concurrent enqueue invalidates an old reorder instead of omitting the new job.
        sqlx::query("INSERT INTO diagnostic_jobs(id,requested_by,state,created_at) VALUES('new','admin','queued',1)").execute(&pool).await?;
        assert!(
            !reorder(
                &pool,
                &QueueOrder {
                    revision: updated.revision,
                    waiting: order.clone()
                }
            )
            .await?
        );
        pool.close().await;
        let pool = crate::db::connect(directory.path()).await?;
        let reopened = snapshot(&pool).await?;
        assert!(reopened.paused);
        assert_eq!(reopened.maximum, 2);
        assert_eq!(
            reopened.jobs[..6]
                .iter()
                .map(|job| job.key)
                .collect::<Vec<_>>(),
            order
        );
        assert_eq!(reopened.jobs[6].id, "new");
        assert!(
            settings(
                &pool,
                &QueueSettings {
                    revision: reopened.revision,
                    paused: false,
                    maximum: 2
                }
            )
            .await?
        );
        let publication = claim(&pool, order[0]).await?.unwrap();
        assert_eq!(publication.kind, "publication");
        assert_eq!(publication.row.try_get::<String, _>("state")?, "publishing");
        pool.close().await;
        Ok(())
    }

    #[tokio::test]
    async fn processing_queue_global_cap_lowering_drains_and_claims_are_atomic() -> Result<()> {
        let (_directory, pool) = fixture().await?;
        let initial = snapshot(&pool).await?;
        assert!(
            settings(
                &pool,
                &QueueSettings {
                    revision: initial.revision,
                    paused: false,
                    maximum: 2
                }
            )
            .await?
        );
        let key = initial.jobs[0].key;
        let (first, duplicate) = tokio::join!(claim(&pool, key), claim(&pool, key));
        assert_ne!(first?.is_some(), duplicate?.is_some());
        let scan = claim(&pool, initial.jobs[1].key).await?.unwrap();
        assert_eq!(scan.kind, "scan");
        assert!(claim(&pool, initial.jobs[2].key).await?.is_none());
        let current = snapshot(&pool).await?;
        assert!(
            settings(
                &pool,
                &QueueSettings {
                    revision: current.revision,
                    paused: false,
                    maximum: 1
                }
            )
            .await?
        );
        assert_eq!(
            snapshot(&pool)
                .await?
                .jobs
                .iter()
                .filter(|job| job.state == "running")
                .count(),
            2
        );
        assert!(
            !reorder(
                &pool,
                &QueueOrder {
                    revision: snapshot(&pool).await?.revision,
                    waiting: vec![key]
                }
            )
            .await?
        );
        finish(&pool, key).await?;
        assert!(claim(&pool, initial.jobs[2].key).await?.is_none());
        finish(&pool, scan.key).await?;
        assert!(claim(&pool, initial.jobs[2].key).await?.is_some());
        assert!(
            settings(
                &pool,
                &QueueSettings {
                    revision: 0,
                    paused: false,
                    maximum: 0
                }
            )
            .await
            .is_err()
        );
        assert!(
            settings(
                &pool,
                &QueueSettings {
                    revision: 0,
                    paused: false,
                    maximum: 17
                }
            )
            .await
            .is_err()
        );
        pool.close().await;
        Ok(())
    }

    #[tokio::test]
    async fn processing_queue_cancel_claim_race_drains_before_ack_and_stops_simple() -> Result<()> {
        let (_directory, pool) = fixture().await?;
        let scan = snapshot(&pool)
            .await?
            .jobs
            .into_iter()
            .find(|job| job.kind == "scan")
            .unwrap();
        let (claimed, cancelled) = tokio::join!(claim(&pool, scan.key), cancel(&pool, scan.key));
        let claimed = claimed?;
        assert!(cancelled?);
        assert!(claim(&pool, scan.key).await?.is_none());
        crate::simple_imports::advance(&pool).await?;
        assert_eq!(
            sqlx::query_scalar::<_, String>("SELECT simple_state FROM scan_jobs")
                .fetch_one(&pool)
                .await?,
            "manual"
        );
        let queue_state: String =
            sqlx::query_scalar("SELECT state FROM processing_queue WHERE key=?")
                .bind(scan.key)
                .fetch_one(&pool)
                .await?;
        if claimed.is_some() {
            assert_eq!(queue_state, "cancelling");
            assert!(
                !crate::scans::list(&pool).await?[0]
                    .error
                    .as_deref()
                    .is_some_and(|error| error.starts_with("Cancelled"))
            );
            sqlx::query("UPDATE scan_jobs SET state='needs_review' WHERE id='scan'")
                .execute(&pool)
                .await?;
            assert_ne!(
                sqlx::query_scalar::<_, String>("SELECT state FROM scan_jobs")
                    .fetch_one(&pool)
                    .await?,
                "needs_review"
            );
            finish(&pool, scan.key).await?;
        } else {
            assert_eq!(queue_state, "cancelled");
        }
        assert_eq!(
            crate::scans::list(&pool).await?[0].state,
            wetube_core::ScanState::Cancelled
        );
        assert!(!cancel(&pool, scan.key).await?);
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM probe_jobs WHERE scan_job_id='scan'"
            )
            .fetch_one(&pool)
            .await?,
            0
        );
        pool.close().await;
        Ok(())
    }

    #[tokio::test]
    async fn processing_queue_startup_restores_all_stage_results_without_reviving_cancelled_work()
    -> Result<()> {
        for cancelled in [false, true] {
            let (directory, pool) = fixture().await?;
            let _owner = crate::lock::MaintenanceLock::acquire(directory.path())?;
            let jobs = snapshot(&pool).await?.jobs;
            let settings = snapshot(&pool).await?;
            super::settings(
                &pool,
                &QueueSettings {
                    revision: settings.revision,
                    paused: false,
                    maximum: 16,
                },
            )
            .await?;
            for job in &jobs {
                claim(&pool, job.key).await?.unwrap();
                defer_finish(
                    &pool,
                    &job.kind,
                    &job.id,
                    match job.kind.as_str() {
                        "diagnostic" | "comparison" => "succeeded",
                        "publication" => "failed",
                        _ => "needs_review",
                    },
                )
                .await?;
                if cancelled {
                    assert!(cancel(&pool, job.key).await?);
                }
            }
            pool.close().await;
            let pool = crate::db::connect(directory.path()).await?;
            restore_results(&pool).await?;
            recover(&pool).await?;
            for job in &jobs {
                let row = sqlx::query(&format!("SELECT * FROM {} WHERE id=?", table(&job.kind)?))
                    .bind(&job.id)
                    .fetch_one(&pool)
                    .await?;
                assert_eq!(
                    job_state(&row)?,
                    if cancelled {
                        "cancelled"
                    } else {
                        match job.kind.as_str() {
                            "diagnostic" | "comparison" => "succeeded",
                            "publication" => "failed",
                            _ => "needs_review",
                        }
                    },
                    "{}",
                    job.kind
                );
            }
            pool.close().await;
        }
        Ok(())
    }

    #[tokio::test]
    async fn processing_queue_all_stage_cancellations_keep_attempts_and_reports() -> Result<()> {
        let (_directory, pool) = fixture().await?;
        let jobs = snapshot(&pool).await?.jobs;
        for job in jobs {
            assert!(claim(&pool, job.key).await?.is_some());
            defer_finish(
                &pool,
                &job.kind,
                &job.id,
                match job.kind.as_str() {
                    "diagnostic" | "comparison" => "succeeded",
                    "publication" => "failed",
                    _ => "needs_review",
                },
            )
            .await?;
            let table = table(&job.kind)?;
            let before: Option<String> = if job.kind == "diagnostic" {
                None
            } else {
                sqlx::query_scalar(&format!("SELECT attempt_id FROM {table} WHERE id=?"))
                    .bind(&job.id)
                    .fetch_one(&pool)
                    .await?
            };
            assert!(cancel(&pool, job.key).await?);
            assert!(claim(&pool, job.key).await?.is_none());
            assert!(cancellation_requested(&pool, job.key).await?);
            finish(&pool, job.key).await?;
            let row = sqlx::query(&format!("SELECT * FROM {table} WHERE id=?"))
                .bind(&job.id)
                .fetch_one(&pool)
                .await?;
            assert_eq!(job_state(&row)?, "cancelled");
            if job.kind != "diagnostic" {
                assert_eq!(row.try_get::<Option<String>, _>("attempt_id")?, before);
            }
        }
        assert_eq!(
            sqlx::query_scalar::<_, String>("SELECT report FROM probe_jobs WHERE id='probe'")
                .fetch_one(&pool)
                .await?,
            "{}"
        );
        pool.close().await;
        Ok(())
    }
}
