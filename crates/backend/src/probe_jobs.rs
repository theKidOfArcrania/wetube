use anyhow::Result;
use sqlx::{Row, SqlitePool, sqlite::SqliteRow};
use wetube_core::{ProbeJob, ProbeRequest, ProbeState};

pub fn from_row(row: &SqliteRow) -> Result<ProbeJob> {
    let state: ProbeState =
        serde_json::from_value(serde_json::Value::String(super::queue::job_state(row)?))?;
    Ok(ProbeJob {
        progress: row
            .try_get::<Option<String>, _>("progress")?
            .map(|text| serde_json::from_str(&text))
            .transpose()?,
        id: row.try_get("id")?,
        name: row.try_get("name")?,
        title: u8::try_from(row.try_get::<i64, _>("title")?)?,
        angle: u8::try_from(row.try_get::<i64, _>("angle")?)?,
        state,
        created_at: row.try_get("created_at")?,
        attempt_id: row.try_get("attempt_id")?,
        report: row
            .try_get::<Option<String>, _>("report")?
            .map(|report| serde_json::from_str(&report))
            .transpose()?,
        error: row.try_get("error")?,
    })
}

pub async fn list(pool: &SqlitePool) -> Result<Vec<ProbeJob>> {
    let rows =
        sqlx::query("SELECT * FROM probe_jobs ORDER BY created_at DESC, rowid DESC LIMIT 20")
            .fetch_all(pool)
            .await?;
    rows.iter().map(from_row).collect()
}

pub async fn enqueue(
    pool: &SqlitePool,
    user_id: &str,
    request: &ProbeRequest,
) -> Result<Option<ProbeJob>> {
    let id = uuid::Uuid::new_v4().to_string();
    let row = sqlx::query(
        "INSERT INTO probe_jobs (id, requested_by, name, source_identity, title, angle, state, created_at)
         SELECT ?, ?, ?, ?, ?, ?, 'queued', ? WHERE
         (SELECT COUNT(*) FROM processing_queue WHERE state='waiting')<100
         AND NOT EXISTS
         (SELECT 1 FROM probe_jobs WHERE state IN ('queued', 'staging', 'probing')
          AND name=? AND source_identity=? AND title=? AND angle=?) RETURNING *",
    )
    .bind(id).bind(user_id).bind(&request.name).bind(&request.identity)
    .bind(i64::from(request.title)).bind(i64::from(request.angle)).bind(super::now())
    .bind(&request.name).bind(&request.identity).bind(request.title).bind(request.angle)
    .fetch_optional(pool).await?;
    row.as_ref().map(from_row).transpose()
}
