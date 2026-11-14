use anyhow::{Result, ensure};
use sqlx::{Row, SqlitePool, sqlite::SqliteRow};
use wetube_core::{ConversionJob, ConversionRequest, PublishRequest};

pub fn from_row(row: &SqliteRow) -> Result<ConversionJob> {
    Ok(ConversionJob {
        progress: row
            .try_get::<Option<String>, _>("progress")?
            .map(|text| serde_json::from_str(&text))
            .transpose()?,
        id: row.try_get("id")?,
        probe_job_id: row.try_get("probe_job_id")?,
        state: serde_json::from_value(row.try_get::<String, _>("state")?.into())?,
        created_at: row.try_get("created_at")?,
        attempt_id: row.try_get("attempt_id")?,
        video_treatment: serde_json::from_value(
            row.try_get::<String, _>("video_treatment")?.into(),
        )?,
        phase: row.try_get("phase")?,
        report: row
            .try_get::<Option<String>, _>("report")?
            .map(|text| serde_json::from_str(&text))
            .transpose()?,
        error: row.try_get("error")?,
        movie_id: row.try_get("movie_id")?,
        movie_sha256: row.try_get("movie_sha256")?,
    })
}

pub async fn list(pool: &SqlitePool) -> Result<Vec<ConversionJob>> {
    sqlx::query("SELECT * FROM conversion_jobs ORDER BY created_at DESC, rowid DESC LIMIT 20")
        .fetch_all(pool)
        .await?
        .iter()
        .map(from_row)
        .collect()
}

pub async fn enqueue(
    pool: &SqlitePool,
    user: &str,
    request: &ConversionRequest,
) -> Result<Option<ConversionJob>> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let treatment = serde_json::to_value(request.video_treatment)?;
    let row = sqlx::query(
        "INSERT INTO conversion_jobs (id,probe_job_id,requested_by,state,created_at,video_treatment)
         SELECT ?,id,?,'queued',?,? FROM probe_jobs
         WHERE id=? AND state='needs_review' AND attempt_id IS NOT NULL AND report IS NOT NULL
         AND NOT EXISTS (SELECT 1 FROM scan_jobs s WHERE s.id=probe_jobs.scan_job_id
                         AND s.simple_state IN ('scan','inspection','conversion'))
         AND NOT EXISTS (SELECT 1 FROM conversion_jobs WHERE probe_job_id=? AND state!='failed')
         ON CONFLICT DO NOTHING RETURNING *"
    ).bind(uuid::Uuid::new_v4().to_string()).bind(user).bind(super::now())
        .bind(treatment.as_str().unwrap()).bind(&request.probe_job_id).bind(&request.probe_job_id)
        .fetch_optional(&mut *tx).await?;
    if row.is_some() {
        sqlx::query("UPDATE scan_jobs SET simple_state='manual' WHERE id=(SELECT scan_job_id FROM probe_jobs WHERE id=?) AND simple_state='advanced'")
            .bind(&request.probe_job_id).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    row.as_ref().map(from_row).transpose()
}

pub async fn publish(
    pool: &SqlitePool,
    id: &str,
    request: &PublishRequest,
) -> Result<Option<ConversionJob>> {
    ensure!(
        request.confirm
            && !request.title.trim().is_empty()
            && request.title.len() <= 512
            && request.description.len() <= 16384
            && request
                .year
                .is_none_or(|year| (1888..=2100).contains(&year))
            && request.movie_sha256.len() == 64
            && request
                .movie_sha256
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit()),
        "publication requires confirmation and valid metadata/hash"
    );
    if let Some(source) = &request.tmdb {
        super::metadata::validate_selection(source, super::now())?;
    }
    let row = sqlx::query(
        "UPDATE conversion_jobs SET state='queued_publication',phase='queued_publication',publication=?
         WHERE id=? AND state='needs_review' AND movie_sha256=? RETURNING *"
    ).bind(serde_json::to_string(request)?).bind(id).bind(&request.movie_sha256)
        .fetch_optional(pool).await?;
    row.as_ref().map(from_row).transpose()
}

#[cfg(test)]
mod tests {
    use super::*;
    use wetube_core::{ConversionState, VideoTreatment};

    #[tokio::test]
    async fn only_reviewed_probes_and_bound_hashes_can_be_queued_once() {
        let dir = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        super::super::db::initialize(dir.path()).await.unwrap();
        let pool = super::super::db::connect(dir.path()).await.unwrap();
        sqlx::query("INSERT INTO users VALUES ('admin','admin','hash','admin')")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO probe_jobs (id,requested_by,name,source_identity,title,angle,state,created_at,attempt_id,report) VALUES ('probe','admin','a.iso','identity',1,1,'failed',1,'attempt','{}')")
            .execute(&pool).await.unwrap();
        let request = ConversionRequest {
            probe_job_id: "probe".into(),
            video_treatment: VideoTreatment::Preserve,
        };
        assert!(enqueue(&pool, "admin", &request).await.unwrap().is_none());
        sqlx::query("UPDATE probe_jobs SET state='needs_review'")
            .execute(&pool)
            .await
            .unwrap();
        let job = enqueue(&pool, "admin", &request).await.unwrap().unwrap();
        assert!(enqueue(&pool, "admin", &request).await.unwrap().is_none());
        let mut publication = PublishRequest {
            title: "Movie".into(),
            year: None,
            description: "".into(),
            movie_sha256: "b".repeat(64),
            confirm: true,
            tmdb: None,
        };
        assert!(
            publish(&pool, &job.id, &publication)
                .await
                .unwrap()
                .is_none()
        );
        sqlx::query("UPDATE conversion_jobs SET state='needs_review',movie_sha256=?")
            .bind("a".repeat(64))
            .execute(&pool)
            .await
            .unwrap();
        assert!(
            publish(&pool, &job.id, &publication)
                .await
                .unwrap()
                .is_none()
        );
        publication.movie_sha256 = "a".repeat(64);
        assert_eq!(
            publish(&pool, &job.id, &publication)
                .await
                .unwrap()
                .unwrap()
                .state,
            ConversionState::QueuedPublication
        );
        assert!(
            publish(&pool, &job.id, &publication)
                .await
                .unwrap()
                .is_none()
        );
        pool.close().await;
    }
}
