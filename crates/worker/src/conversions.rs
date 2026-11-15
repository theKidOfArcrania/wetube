use anyhow::{Context, Result, ensure};
use sqlx::{Row, SqlitePool};
use std::{sync::Arc, time::Duration};
use wetube_backend::{conversions, preview::PreviewRoot, probe_jobs};
use wetube_core::PublishRequest;

#[derive(Clone)]
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
    sqlx::query("UPDATE conversion_jobs SET state='failed',error='Worker interrupted. All attempts/packages retained; no automatic retry or publication.' WHERE state IN ('converting','publishing')
        AND NOT EXISTS(SELECT 1 FROM processing_queue WHERE job_id=conversion_jobs.id AND kind IN ('conversion','publication') AND state IN ('running','cancelling') AND finish_state IS NOT NULL)")
        .execute(pool).await?;
    wetube_backend::metadata::expire(pool, wetube_backend::now()).await?;
    Ok(())
}

pub async fn tick(
    pool: &SqlitePool,
    config: Arc<super::imports::ImportConfig>,
    media: &PreviewRoot,
    limits: &Limits,
) -> Result<bool> {
    limits.validate()?;
    wetube_backend::metadata::expire(pool, wetube_backend::now()).await?;
    let claim = match wetube_backend::queue::next(pool, "conversion").await? {
        Some(claim) => Some(claim),
        None => wetube_backend::queue::next(pool, "publication").await?,
    };
    let Some(claim) = claim else {
        return Ok(false);
    };
    let key = claim.key;
    let result = run(pool, config, media, limits, claim.row).await;
    wetube_backend::queue::finish(pool, key).await?;
    result
}

pub(crate) async fn run(
    pool: &SqlitePool,
    config: Arc<super::imports::ImportConfig>,
    media: &PreviewRoot,
    limits: &Limits,
    row: sqlx::sqlite::SqliteRow,
) -> Result<bool> {
    let job = conversions::from_row(&row)?;
    let recording = super::progress::Recording::start(
        pool,
        "conversion_jobs",
        &job.id,
        wetube_core::MediaWorkflow::Conversion,
        if job.state == wetube_core::ConversionState::Converting {
            0
        } else {
            5
        },
    );
    let progress = &recording.progress;
    let result: Result<()> = async {
        if job.state == wetube_core::ConversionState::Converting {
            wetube_backend::simple_imports::validate_conversion(pool, &job.id).await?;
            let probe = sqlx::query("SELECT * FROM probe_jobs WHERE id=? AND state='needs_review'")
                .bind(&job.probe_job_id).fetch_one(pool).await?;
            let probe = probe_jobs::from_row(&probe)?;
            let report = super::experimental::convert(pool,config.clone(),&job,&probe,limits,progress).await?;
            sqlx::query("UPDATE conversion_jobs SET report=? WHERE id=?")
                .bind(report.to_string()).bind(&job.id).execute(pool).await?;
            ensure!(report["status"] == "verified_experimental_output", "{}",report["error"].as_str().unwrap_or("conversion failed"));
            let digest = report["movie_sha256"].as_str().context("successful conversion missing digest")?;
            sqlx::query("UPDATE conversion_jobs SET movie_sha256=?,error=NULL WHERE id=? AND state='converting'")
                .bind(digest).bind(&job.id).execute(pool).await?;
            wetube_backend::queue::defer_finish(pool,"conversion",&job.id,"needs_review").await?;
        } else {
            let publication: PublishRequest = serde_json::from_str(&row.try_get::<String,_>("publication")?)?;
            if let Some(source) = &publication.tmdb {
                ensure!(wetube_backend::metadata::expires_at(source)? > wetube_backend::now(), "Movie metadata expired; review metadata again.");
            }
            ensure!(publication.confirm && Some(&publication.movie_sha256) == job.movie_sha256.as_ref(), "publication confirmation/hash missing");
            let attempt = uuid::Uuid::parse_str(job.attempt_id.as_deref().context("missing successful attempt")?)?;
            let report = config.scratch_path().join(attempt.to_string()).join("report.json");
            super::admission::admit_conversion(pool,media,super::admission::Admission {
                report:&report,title:&publication.title,year:publication.year,description:&publication.description,
                movie_sha256:&publication.movie_sha256,max_bytes:limits.max_output_bytes,timeout:limits.timeout,
            },&job.id,progress).await?;
        }
        Ok(())
    }.await;
    if let Err(error) = result {
        let error: String = format!("{error:#}").chars().take(4096).collect();
        sqlx::query("UPDATE conversion_jobs SET error=? WHERE id=? AND state!='published'")
            .bind(error)
            .bind(&job.id)
            .execute(pool)
            .await?;
        wetube_backend::queue::defer_finish(
            pool,
            if job.state == wetube_core::ConversionState::Converting {
                "conversion"
            } else {
                "publication"
            },
            &job.id,
            "failed",
        )
        .await?;
    }
    recording.finish().await;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn simple_claim_gate_failure_is_retained_without_media_work_or_worker_failure()
    -> Result<()> {
        let directory = tempfile::tempdir()?;
        let state = directory.path().join("state");
        let incoming = directory.path().join("incoming");
        let scratch = directory.path().join("scratch");
        let media = directory.path().join("media");
        for path in [&state, &incoming, &media] {
            std::fs::create_dir(path)?;
        }
        wetube_backend::db::initialize(&state).await?;
        let pool = wetube_backend::db::connect(&state).await?;
        sqlx::query("INSERT INTO users VALUES('admin','admin','hash','viewer')")
            .execute(&pool)
            .await?;
        sqlx::query("INSERT INTO scan_jobs(id,requested_by,name,source_identity,state,created_at,simple_state,simple_requested_by) VALUES('scan','admin','fixture.iso','bound','needs_review',1,'conversion','admin')")
            .execute(&pool).await?;
        sqlx::query("INSERT INTO probe_jobs(id,requested_by,name,source_identity,title,angle,state,created_at,scan_job_id,attempt_id,report) VALUES('probe','admin','fixture.iso','bound',1,1,'needs_review',1,'scan','retained','{}')")
            .execute(&pool).await?;
        sqlx::query("INSERT INTO conversion_jobs(id,probe_job_id,requested_by,state,created_at,video_treatment) VALUES('conversion','probe','admin','queued',1,'preserve')")
            .execute(&pool).await?;
        sqlx::query(
            "UPDATE scan_jobs SET simple_probe_id='probe',simple_conversion_id='conversion'",
        )
        .execute(&pool)
        .await?;
        wetube_backend::simple_imports::advance(&pool).await?;
        let config = Arc::new(super::super::imports::ImportConfig::open(
            &incoming,
            &scratch,
            1024,
            1024 * 1024,
            Duration::from_secs(30),
        )?);
        let media = PreviewRoot::open(&media)?;
        assert!(
            tick(
                &pool,
                config,
                &media,
                &Limits {
                    timeout: Duration::from_secs(30),
                    threads: 2,
                    max_output_bytes: 1024 * 1024,
                }
            )
            .await?
        );
        let job = conversions::list(&pool).await?.remove(0);
        assert_eq!(job.state, wetube_core::ConversionState::Failed);
        assert!(
            job.error
                .unwrap()
                .contains("authorization or workflow changed")
        );
        assert_eq!(
            std::fs::read_dir(scratch)?.count(),
            0,
            "no encoding attempt or output is created after a gate failure"
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM movies")
                .fetch_one(&pool)
                .await?,
            0
        );
        pool.close().await;
        Ok(())
    }

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
