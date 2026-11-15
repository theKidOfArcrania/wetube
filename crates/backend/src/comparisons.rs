use anyhow::Result;
use sqlx::{Row, SqlitePool, sqlite::SqliteRow};
use wetube_core::ComparisonJob;

pub fn from_row(row: &SqliteRow) -> Result<ComparisonJob> {
    Ok(ComparisonJob {
        id: row.try_get("id")?,
        probe_job_id: row.try_get("probe_job_id")?,
        state: serde_json::from_value(super::queue::job_state(row)?.into())?,
        created_at: row.try_get("created_at")?,
        attempt_id: row.try_get("attempt_id")?,
        report: row
            .try_get::<Option<String>, _>("report")?
            .map(|report| serde_json::from_str(&report))
            .transpose()?,
        error: row.try_get("error")?,
        progress: row
            .try_get::<Option<String>, _>("progress")?
            .and_then(|value| serde_json::from_str(&value).ok()),
    })
}

pub async fn latest(pool: &SqlitePool, probe: &str) -> Result<Option<ComparisonJob>> {
    sqlx::query(
        "SELECT c.* FROM comparison_jobs c JOIN probe_jobs p ON p.id=c.probe_job_id
        WHERE p.id=? AND p.state='needs_review' AND c.probe_report=p.report
        AND c.title=p.title AND c.angle=p.angle ORDER BY c.created_at DESC,c.rowid DESC LIMIT 1",
    )
    .bind(probe)
    .fetch_optional(pool)
    .await?
    .as_ref()
    .map(from_row)
    .transpose()
}

pub async fn enqueue(pool: &SqlitePool, user: &str, probe: &str) -> Result<Option<ComparisonJob>> {
    let row = sqlx::query("INSERT INTO comparison_jobs
        (id,probe_job_id,requested_by,state,created_at,probe_report,title,angle)
        SELECT ?,id,?,'queued',?,report,title,angle FROM probe_jobs p
        WHERE id=? AND state='needs_review' AND report IS NOT NULL AND attempt_id IS NOT NULL
        AND (SELECT COUNT(*) FROM processing_queue WHERE state='waiting')<100
        AND NOT EXISTS (SELECT 1 FROM comparison_jobs c WHERE c.probe_job_id=p.id
            AND (c.state IN ('queued','running') OR
                 (c.state='succeeded' AND c.probe_report=p.report AND c.title=p.title AND c.angle=p.angle)))
        ON CONFLICT DO NOTHING RETURNING *")
        .bind(uuid::Uuid::new_v4().to_string()).bind(user).bind(super::now()).bind(probe)
        .fetch_optional(pool).await?;
    row.as_ref().map(from_row).transpose()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn queue_is_bounded_and_concurrent_claims_are_deduplicated() -> Result<()> {
        let directory = tempfile::tempdir()?;
        crate::db::initialize(directory.path()).await?;
        let pool = crate::db::connect(directory.path()).await?;
        sqlx::query("INSERT INTO users VALUES ('admin','admin','hash','admin')")
            .execute(&pool)
            .await?;
        for id in ["one", "two", "three", "four", "five"] {
            sqlx::query("INSERT INTO probe_jobs(id,requested_by,name,source_identity,title,angle,state,created_at,attempt_id,report)
                VALUES (?,'admin','a.iso','bound',1,1,'needs_review',1,'retained','{}')").bind(id).execute(&pool).await?;
        }
        let (first, duplicate) = tokio::join!(
            enqueue(&pool, "admin", "one"),
            enqueue(&pool, "admin", "one")
        );
        assert_ne!(first?.is_some(), duplicate?.is_some());
        let first_key = crate::queue::snapshot(&pool).await?.jobs[0].key;
        for id in ["two", "three", "four", "five"] {
            assert!(enqueue(&pool, "admin", id).await?.is_some());
        }
        for index in 6..=101 {
            let id = format!("probe-{index}");
            sqlx::query("INSERT INTO probe_jobs(id,requested_by,name,source_identity,title,angle,state,created_at,attempt_id,report)
                VALUES (?,'admin','a.iso','bound',1,1,'needs_review',1,'retained','{}')").bind(&id).execute(&pool).await?;
            assert_eq!(enqueue(&pool, "admin", &id).await?.is_some(), index <= 100);
        }
        assert!(crate::queue::cancel(&pool, first_key).await?);
        assert!(enqueue(&pool, "admin", "probe-101").await?.is_some());
        pool.close().await;
        Ok(())
    }
}
