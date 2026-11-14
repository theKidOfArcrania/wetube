use anyhow::{Context, Result, ensure};
use sqlx::{Row, SqlitePool};
use std::{sync::Arc, time::Duration};
use wetube_backend::{conversions, preview::PreviewRoot, probe_jobs};
use wetube_core::PublishRequest;

pub struct Limits {
    pub timeout: Duration,
    pub threads: u8,
    pub max_output_bytes: u64,
}

impl Limits {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.timeout >= Duration::from_secs(1)
                && self.timeout <= Duration::from_secs(86400)
                && (1..=16).contains(&self.threads)
                && (1024 * 1024..=64 * 1024 * 1024 * 1024).contains(&self.max_output_bytes),
            "invalid conversion resource limits"
        );
        Ok(())
    }
}

pub async fn recover(pool: &SqlitePool) -> Result<()> {
    // Publication and registration commit together. An interrupted pre-commit
    // package is deliberately not retried or made ready.
    sqlx::query("UPDATE conversion_jobs SET state='failed',error='Worker interrupted. All attempts/packages retained; no automatic retry or publication.' WHERE state IN ('converting','publishing')")
        .execute(pool).await?;
    Ok(())
}

pub async fn tick(
    pool: &SqlitePool,
    config: Arc<super::imports::ImportConfig>,
    media: &PreviewRoot,
    limits: &Limits,
) -> Result<bool> {
    limits.validate()?;
    let row = sqlx::query(
        "UPDATE conversion_jobs SET state=CASE state WHEN 'queued' THEN 'converting' ELSE 'publishing' END,
         phase=CASE state WHEN 'queued' THEN 'revalidating_input' ELSE 'publishing' END,
         attempt_id=CASE state WHEN 'queued' THEN ? ELSE attempt_id END
         WHERE id=(SELECT id FROM conversion_jobs WHERE state IN ('queued','queued_publication') ORDER BY created_at,rowid LIMIT 1)
         RETURNING *"
    ).bind(uuid::Uuid::new_v4().to_string()).fetch_optional(pool).await?;
    let Some(row) = row else {
        return Ok(false);
    };
    let job = conversions::from_row(&row)?;
    let result: Result<()> = async {
        if job.state == wetube_core::ConversionState::Converting {
            let probe = sqlx::query("SELECT * FROM probe_jobs WHERE id=? AND state='needs_review'")
                .bind(&job.probe_job_id).fetch_one(pool).await?;
            let probe = probe_jobs::from_row(&probe)?;
            let report = super::experimental::convert(pool,config.clone(),&job,&probe,limits).await?;
            sqlx::query("UPDATE conversion_jobs SET report=? WHERE id=?")
                .bind(report.to_string()).bind(&job.id).execute(pool).await?;
            ensure!(report["status"] == "verified_experimental_output", "{}",report["error"].as_str().unwrap_or("conversion failed"));
            let digest = report["movie_sha256"].as_str().context("successful conversion missing digest")?;
            sqlx::query("UPDATE conversion_jobs SET state='needs_review',phase='awaiting_review',movie_sha256=?,error=NULL WHERE id=? AND state='converting'")
                .bind(digest).bind(&job.id).execute(pool).await?;
        } else {
            let publication: PublishRequest = serde_json::from_str(&row.try_get::<String,_>("publication")?)?;
            ensure!(publication.confirm && Some(&publication.movie_sha256) == job.movie_sha256.as_ref(), "publication confirmation/hash missing");
            let attempt = uuid::Uuid::parse_str(job.attempt_id.as_deref().context("missing successful attempt")?)?;
            let report = config.scratch_path().join(attempt.to_string()).join("report.json");
            super::admission::admit_conversion(pool,media,super::admission::Admission {
                report:&report,title:&publication.title,year:publication.year,description:&publication.description,
                movie_sha256:&publication.movie_sha256,max_bytes:limits.max_output_bytes,timeout:limits.timeout,
            },&job.id).await?;
        }
        Ok(())
    }.await;
    if let Err(error) = result {
        let error: String = format!("{error:#}").chars().take(4096).collect();
        sqlx::query(
            "UPDATE conversion_jobs SET state='failed',error=? WHERE id=? AND state!='published'",
        )
        .bind(error)
        .bind(&job.id)
        .execute(pool)
        .await?;
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn interrupted_work_is_retained_failed_and_never_registered() -> Result<()> {
        let dir = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR"))?;
        wetube_backend::db::initialize(dir.path()).await?;
        let pool = wetube_backend::db::connect(dir.path()).await?;
        sqlx::query("INSERT INTO users VALUES ('admin','admin','hash','admin')")
            .execute(&pool)
            .await?;
        sqlx::query("INSERT INTO probe_jobs (id,requested_by,name,source_identity,title,angle,state,created_at) VALUES ('probe','admin','a.iso','hash',1,1,'needs_review',1)").execute(&pool).await?;
        sqlx::query("INSERT INTO conversion_jobs(id,probe_job_id,requested_by,state,created_at,video_treatment,attempt_id,report) VALUES('job','probe','admin','publishing',1,'preserve','retained','{}')").execute(&pool).await?;
        let artifacts = tempfile::tempdir()?;
        let retained = artifacts.path().join("retained");
        std::fs::write(&retained, b"original remains")?;
        recover(&pool).await?;
        let job = conversions::list(&pool).await?.remove(0);
        assert_eq!(job.state, wetube_core::ConversionState::Failed);
        assert_eq!(std::fs::read(retained)?, b"original remains");
        assert_eq!(job.report, Some(serde_json::json!({})));
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM movies")
            .fetch_one(&pool)
            .await?;
        assert_eq!(count, 0);
        pool.close().await;
        Ok(())
    }
}
