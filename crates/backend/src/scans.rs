use anyhow::{Context, Result, ensure};
use sqlx::{Row, SqlitePool, sqlite::SqliteRow};
use wetube_core::{InspectRequest, ProbeJob, ScanJob, ScanRequest};

pub fn from_row(row: &SqliteRow) -> Result<ScanJob> {
    Ok(ScanJob {
        progress: row
            .try_get::<Option<String>, _>("progress")?
            .map(|text| serde_json::from_str(&text))
            .transpose()?,
        id: row.try_get("id")?,
        name: row.try_get("name")?,
        state: serde_json::from_value(row.try_get::<String, _>("state")?.into())?,
        created_at: row.try_get("created_at")?,
        attempt_id: row.try_get("attempt_id")?,
        report: row
            .try_get::<Option<String>, _>("report")?
            .map(|report| serde_json::from_str(&report))
            .transpose()?,
        error: row.try_get("error")?,
    })
}

pub async fn list(pool: &SqlitePool) -> Result<Vec<ScanJob>> {
    sqlx::query("SELECT * FROM scan_jobs ORDER BY created_at DESC,rowid DESC LIMIT 20")
        .fetch_all(pool)
        .await?
        .iter()
        .map(from_row)
        .collect()
}

pub async fn enqueue(
    pool: &SqlitePool,
    user: &str,
    request: &ScanRequest,
) -> Result<Option<ScanJob>> {
    let row = sqlx::query(
        "INSERT INTO scan_jobs(id,requested_by,name,source_identity,state,created_at)
         SELECT ?,?,?,?,'queued',? WHERE NOT EXISTS
         (SELECT 1 FROM scan_jobs WHERE state IN ('queued','staging','scanning'))
         RETURNING *",
    )
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(user)
    .bind(&request.name)
    .bind(&request.identity)
    .bind(super::now())
    .fetch_optional(pool)
    .await?;
    row.as_ref().map(from_row).transpose()
}

pub async fn inspect(
    pool: &SqlitePool,
    user: &str,
    id: &str,
    request: &InspectRequest,
) -> Result<Option<ProbeJob>> {
    let scan = sqlx::query("SELECT * FROM scan_jobs WHERE id=? AND state='needs_review'")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    let Some(scan) = scan else {
        return Ok(None);
    };
    let job = from_row(&scan)?;
    ensure!(
        job.report.as_ref().is_some_and(|report| report
            .titles
            .iter()
            .any(|title| title.title == request.title && title.angles.contains(&request.angle))),
        "title/angle was not discovered in the scanned DVD"
    );
    let row = sqlx::query(
        "INSERT INTO probe_jobs(id,requested_by,name,source_identity,title,angle,state,created_at,attempt_id,scan_job_id,report)
         SELECT ?,?,name,source_identity,?,?,'queued',?,?,id,?
         FROM scan_jobs WHERE id=? AND state='needs_review'
         AND NOT EXISTS(SELECT 1 FROM probe_jobs WHERE state IN ('queued','staging','probing'))
         ON CONFLICT DO NOTHING RETURNING *"
    ).bind(uuid::Uuid::new_v4().to_string()).bind(user).bind(request.title).bind(request.angle).bind(super::now())
        .bind(job.attempt_id.as_ref().context("scan has no retained attempt")?)
        .bind(serde_json::json!({"sha256":scan.try_get::<String,_>("sha256")?,"size_bytes":scan.try_get::<i64,_>("size_bytes")?,
            "staged_attempt_id":job.attempt_id}).to_string()).bind(id)
        .fetch_optional(pool).await?;
    row.as_ref().map(super::probe_jobs::from_row).transpose()
}
