use anyhow::{Context, Result, ensure};
use sqlx::{Row, SqlitePool, sqlite::SqliteRow};
use wetube_core::{DvdScanReport, ScanJob, SimpleImportRequest};

/// Authorization is recorded once; worker continuation never relies on a browser callback.
pub async fn enqueue(
    pool: &SqlitePool,
    user: &str,
    request: &SimpleImportRequest,
) -> Result<Option<ScanJob>> {
    ensure!(
        request.scan.copy_complete && request.accept_preserve,
        "Confirm the finished copy and accept the Preserve preview treatment."
    );
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let existing = sqlx::query(
        "SELECT * FROM scan_jobs WHERE name=? AND source_identity=? AND simple_state IS NOT NULL",
    )
    .bind(&request.scan.name)
    .bind(&request.scan.identity)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(row) = existing {
        return Ok(Some(super::scans::from_row(&row)?));
    }
    let reusable = sqlx::query(
        "SELECT * FROM scan_jobs s WHERE name=? AND source_identity=? AND error IS NULL
         AND state IN ('queued','staging','scanning','needs_review') AND simple_state IS NULL
         ORDER BY created_at DESC,rowid DESC LIMIT 1",
    )
    .bind(&request.scan.name)
    .bind(&request.scan.identity)
    .fetch_optional(&mut *tx)
    .await?;
    let row = if let Some(scan) = reusable {
        let id: String = scan.try_get("id")?;
        let probes = sqlx::query(
            "SELECT * FROM probe_jobs WHERE scan_job_id=? ORDER BY created_at DESC,rowid DESC",
        )
        .bind(&id)
        .fetch_all(&mut *tx)
        .await?;
        let converted: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM conversion_jobs c JOIN probe_jobs p ON p.id=c.probe_job_id WHERE p.scan_job_id=?)",
        ).bind(&id).fetch_one(&mut *tx).await?;
        let active = probes.iter().any(|probe| {
            matches!(
                probe.get::<&str, _>("state"),
                "queued" | "staging" | "probing"
            )
        });
        let inspected = (!converted && !active)
            .then(|| {
                probes
                    .iter()
                    .find(|probe| inspection_gate(&scan, probe).is_ok())
            })
            .flatten();
        let (state, message) = if converted {
            (
                "manual",
                Some("Existing conversion retained. Continue its review in Advanced."),
            )
        } else if active || (!probes.is_empty() && inspected.is_none()) {
            (
                "advanced",
                Some(
                    "Existing inspection retained. Continue in Advanced with its results; no new copy or scan was queued.",
                ),
            )
        } else if inspected.is_some() {
            ("inspection", None)
        } else {
            ("scan", None)
        };
        // Never replace an in-progress manual selection or retry a failed child.
        sqlx::query("UPDATE scan_jobs SET simple_state=?,simple_message=?,simple_probe_id=? WHERE id=? RETURNING *")
            .bind(state).bind(message).bind(inspected.map(|probe| probe.get::<String,_>("id")))
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?
    } else {
        sqlx::query(
            "INSERT INTO scan_jobs(id,requested_by,name,source_identity,state,created_at,simple_state)
             SELECT ?,?,?,?,'queued',?,'scan'
             WHERE NOT EXISTS(SELECT 1 FROM scan_jobs WHERE state IN ('queued','staging','scanning'))
             RETURNING *",
        )
        .bind(uuid::Uuid::new_v4().to_string())
        .bind(user)
        .bind(&request.scan.name)
        .bind(&request.scan.identity)
        .bind(super::now())
        .fetch_optional(&mut *tx)
        .await?
    };
    // Keep the original scan requester as provenance when another admin authorizes continuation.
    if let Some(row) = &row {
        sqlx::query("UPDATE scan_jobs SET simple_requested_by=? WHERE id=?")
            .bind(user)
            .bind(row.try_get::<String, _>("id")?)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    row.as_ref().map(super::scans::from_row).transpose()
}

pub async fn advanced(pool: &SqlitePool, id: &str) -> Result<Option<ScanJob>> {
    let row = sqlx::query(
        "UPDATE scan_jobs SET simple_state='advanced',
         simple_message='Advanced mode selected. Review the retained results and select a title, angle and treatment.'
         WHERE id=? AND simple_state IN ('scan','inspection','advanced')
         AND simple_conversion_id IS NULL RETURNING *",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    row.as_ref().map(super::scans::from_row).transpose()
}

fn scan_gate(scan: &SqliteRow) -> Result<DvdScanReport> {
    ensure!(
        scan.try_get::<String, _>("state")? == "needs_review"
            && scan.try_get::<Option<String>, _>("error")?.is_none(),
        "Scan did not complete successfully. Use Advanced to review the retained input."
    );
    let report: DvdScanReport = serde_json::from_str(
        &scan
            .try_get::<Option<String>, _>("report")?
            .context("DVD inventory is missing.")?,
    )?;
    report.simple_selection()?;
    uuid::Uuid::parse_str(
        &scan
            .try_get::<Option<String>, _>("attempt_id")?
            .context("Retained scan input is missing.")?,
    )?;
    let sha = scan
        .try_get::<Option<String>, _>("sha256")?
        .context("Scan checksum is missing.")?;
    ensure!(
        sha.len() == 64
            && sha.bytes().all(|byte| byte.is_ascii_hexdigit())
            && scan
                .try_get::<Option<i64>, _>("size_bytes")?
                .is_some_and(|size| size > 0),
        "Scan input validation is incomplete."
    );
    Ok(report)
}

fn inspection_gate(scan: &SqliteRow, probe: &SqliteRow) -> Result<()> {
    let inventory = scan_gate(scan)?;
    ensure!(
        probe.try_get::<String, _>("state")? == "needs_review"
            && probe.try_get::<Option<String>, _>("error")?.is_none()
            && probe.try_get::<i64, _>("title")? == 1
            && probe.try_get::<i64, _>("angle")? == 1
            && probe.try_get::<Option<String>, _>("scan_job_id")? == Some(scan.try_get("id")?)
            && probe.try_get::<String, _>("name")? == scan.try_get::<String, _>("name")?
            && probe.try_get::<String, _>("source_identity")?
                == scan.try_get::<String, _>("source_identity")?
            && probe.try_get::<Option<String>, _>("attempt_id")?
                == scan.try_get::<Option<String>, _>("attempt_id")?,
        "Inspection does not match the successful title 1 / angle 1 scan."
    );
    let report: serde_json::Value = serde_json::from_str(
        &probe
            .try_get::<Option<String>, _>("report")?
            .context("Inspection report is missing.")?,
    )?;
    let inspection = &report["inspection"];
    let metadata = &inspection["metadata"];
    let duration = metadata["format"]["duration"]
        .as_str()
        .and_then(|value| value.parse::<f64>().ok());
    let expected = inventory
        .titles
        .iter()
        .find(|title| title.title == 1)
        .unwrap()
        .duration_seconds;
    ensure!(
        report["sha256"].as_str() == scan.try_get::<Option<String>, _>("sha256")?.as_deref()
            && report["size_bytes"].as_i64() == scan.try_get::<Option<i64>, _>("size_bytes")?
            && report["staged_attempt_id"].as_str()
                == scan.try_get::<Option<String>, _>("attempt_id")?.as_deref()
            && inspection["title"] == 1
            && inspection["angle"] == 1
            && inspection["source_size_bytes"] == report["size_bytes"]
            && metadata["format"]["format_name"] == "dvdvideo"
            && metadata["streams"].as_array().is_some_and(|streams| streams
                .iter()
                .any(|stream| stream["codec_type"] == "video"))
            && duration.is_some_and(|duration| duration.is_finite()
                && duration > 0.0
                && (duration - expected).abs() <= 2.0_f64.max(expected * 0.01)),
        "Inspection metadata or duration does not match the verified scan. Use Advanced to review the reports."
    );
    Ok(())
}

pub async fn validate_scan_authorization(pool: &SqlitePool, scan: &str) -> Result<()> {
    let authorized: bool = sqlx::query_scalar(
        "SELECT NOT EXISTS(SELECT 1 FROM scan_jobs s JOIN users u ON u.id=s.simple_requested_by
         WHERE s.id=? AND u.role!='admin')",
    )
    .bind(scan)
    .fetch_one(pool)
    .await?;
    ensure!(
        authorized,
        "Import authorization changed. Review in Advanced."
    );
    Ok(())
}

/// Called only by the single owning worker, including after interrupted-job recovery.
pub async fn advance(pool: &SqlitePool) -> Result<()> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let scans = sqlx::query("SELECT s.*,u.role FROM scan_jobs s JOIN users u ON u.id=s.simple_requested_by WHERE simple_state IN ('scan','inspection','conversion')")
        .fetch_all(&mut *tx).await?;
    for scan in scans {
        let id: String = scan.try_get("id")?;
        let state: String = scan.try_get("simple_state")?;
        let mut stop = None;
        if scan.try_get::<String, _>("role")? != "admin" {
            stop = Some(
                "Import authorization changed. Use Advanced with an administrator account.".into(),
            );
        } else if state == "scan" {
            match scan.try_get::<&str, _>("state")? {
                "failed" => {
                    stop = Some(
                        "Scan failed. Use Advanced to review the retained input and error.".into(),
                    )
                }
                "needs_review" => match scan_gate(&scan) {
                    Err(error) => stop = Some(error.to_string()),
                    Ok(_) => {
                        let probe_id = uuid::Uuid::new_v4().to_string();
                        let report = serde_json::json!({
                            "sha256":scan.try_get::<String,_>("sha256")?,
                            "size_bytes":scan.try_get::<i64,_>("size_bytes")?,
                            "staged_attempt_id":scan.try_get::<String,_>("attempt_id")?,
                        });
                        let inserted = sqlx::query(
                            "INSERT INTO probe_jobs(id,requested_by,name,source_identity,title,angle,state,created_at,attempt_id,scan_job_id,report)
                             SELECT ?,simple_requested_by,name,source_identity,1,1,'queued',?,attempt_id,id,?
                             FROM scan_jobs WHERE id=? AND simple_state='scan'
                             AND NOT EXISTS(SELECT 1 FROM probe_jobs WHERE state IN ('queued','staging','probing'))
                             AND NOT EXISTS(SELECT 1 FROM probe_jobs WHERE scan_job_id=?)
                             ON CONFLICT DO NOTHING",
                        ).bind(&probe_id).bind(super::now()).bind(report.to_string()).bind(&id).bind(&id)
                            .execute(&mut *tx).await?.rows_affected();
                        if inserted == 1 {
                            sqlx::query("UPDATE scan_jobs SET simple_state='inspection',simple_probe_id=? WHERE id=? AND simple_state='scan'")
                                .bind(probe_id).bind(&id).execute(&mut *tx).await?;
                        } else {
                            let selected: i64 = sqlx::query_scalar(
                                "SELECT COUNT(*) FROM probe_jobs WHERE scan_job_id=?",
                            )
                            .bind(&id)
                            .fetch_one(&mut *tx)
                            .await?;
                            if selected > 0 {
                                stop = Some("A manual inspection already exists. Continue in Advanced with its retained results.".into());
                            }
                        }
                    }
                },
                _ => {}
            }
        } else {
            let probe = sqlx::query("SELECT * FROM probe_jobs WHERE id=?")
                .bind(scan.try_get::<Option<String>, _>("simple_probe_id")?)
                .fetch_optional(&mut *tx)
                .await?;
            if let Some(probe) = probe {
                if probe.try_get::<&str, _>("state")? == "failed" {
                    stop = Some(
                        "Inspection failed. Use Advanced to review the retained report and error."
                            .into(),
                    );
                } else if probe.try_get::<&str, _>("state")? == "needs_review" {
                    if let Err(error) = inspection_gate(&scan, &probe) {
                        stop = Some(error.to_string());
                    } else if state == "inspection" {
                        let conversion_id = uuid::Uuid::new_v4().to_string();
                        let inserted = sqlx::query(
                            "INSERT INTO conversion_jobs(id,probe_job_id,requested_by,state,created_at,video_treatment)
                             VALUES(?,?,?,'queued',?,'preserve') ON CONFLICT DO NOTHING",
                        ).bind(&conversion_id).bind(probe.try_get::<String,_>("id")?)
                            .bind(scan.try_get::<String,_>("simple_requested_by")?).bind(super::now())
                            .execute(&mut *tx).await?.rows_affected();
                        if inserted == 1 {
                            sqlx::query("UPDATE scan_jobs SET simple_state='conversion',simple_conversion_id=? WHERE id=? AND simple_state='inspection'")
                                .bind(conversion_id).bind(&id).execute(&mut *tx).await?;
                        } else {
                            stop = Some("A conversion already exists. Review it in Advanced; no second conversion was queued.".into());
                        }
                    } else {
                        let conversion: Option<String> =
                            sqlx::query_scalar("SELECT state FROM conversion_jobs WHERE id=?")
                                .bind(scan.try_get::<Option<String>, _>("simple_conversion_id")?)
                                .fetch_optional(&mut *tx)
                                .await?;
                        match conversion.as_deref() {
                            Some("failed") | None => stop = Some("Conversion failed. Use Advanced to review the retained report and error.".into()),
                            Some("needs_review" | "queued_publication" | "publishing" | "published") => {
                                sqlx::query("UPDATE scan_jobs SET simple_state='review',simple_message='Conversion complete. Review the validation, warnings and metadata, then approve publication separately.' WHERE id=?")
                                    .bind(&id).execute(&mut *tx).await?;
                            }
                            _ => {}
                        }
                    }
                }
            } else {
                stop = Some(
                    "Inspection is unavailable. Use Advanced to review the retained scan.".into(),
                );
            }
        }
        if let Some(message) = stop {
            sqlx::query("UPDATE scan_jobs SET simple_state='advanced',simple_message=? WHERE id=?")
                .bind(message)
                .bind(&id)
                .execute(&mut *tx)
                .await?;
        }
    }
    tx.commit().await?;
    Ok(())
}

/// Recheck the durable workflow at the actual conversion claim, before any media work.
pub async fn validate_conversion(pool: &SqlitePool, conversion: &str) -> Result<()> {
    let scan = sqlx::query("SELECT s.*,u.role FROM scan_jobs s JOIN users u ON u.id=s.simple_requested_by WHERE simple_conversion_id=?")
        .bind(conversion).fetch_optional(pool).await?;
    if let Some(scan) = scan {
        ensure!(
            scan.try_get::<&str, _>("simple_state")? == "conversion"
                && scan.try_get::<&str, _>("role")? == "admin",
            "Simple import authorization or workflow changed. Review in Advanced."
        );
        let job = sqlx::query("SELECT * FROM conversion_jobs WHERE id=?")
            .bind(conversion)
            .fetch_one(pool)
            .await?;
        ensure!(
            job.try_get::<String, _>("probe_job_id")?
                == scan.try_get::<String, _>("simple_probe_id")?
                && job.try_get::<String, _>("requested_by")?
                    == scan.try_get::<String, _>("simple_requested_by")?
                && job.try_get::<&str, _>("video_treatment")? == "preserve"
                && matches!(job.try_get::<&str, _>("state")?, "queued" | "converting"),
            "Simple conversion claim does not match the authorized program and Preserve treatment."
        );
        let probe = sqlx::query("SELECT * FROM probe_jobs WHERE id=?")
            .bind(scan.try_get::<String, _>("simple_probe_id")?)
            .fetch_one(pool)
            .await?;
        inspection_gate(&scan, &probe)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use wetube_core::{ConversionRequest, InspectRequest, ScanRequest, VideoTreatment};

    fn inventory() -> serde_json::Value {
        serde_json::json!({"titles":[
            {"title":1,"duration_seconds":60.0,"chapter_count":2,"audio_count":1,"subtitle_count":0,
             "audio_languages":["en"],"subtitle_languages":[],"angles":[1,2],"main_feature_candidate":false},
            {"title":2,"duration_seconds":10.0,"chapter_count":1,"audio_count":0,"subtitle_count":0,
             "audio_languages":[],"subtitle_languages":[],"angles":[1],"main_feature_candidate":true}
        ]})
    }

    async fn fixture() -> Result<(tempfile::TempDir, SqlitePool, ScanJob, SimpleImportRequest)> {
        let directory = tempfile::tempdir()?;
        super::super::db::initialize(directory.path()).await?;
        let pool = super::super::db::connect(directory.path()).await?;
        sqlx::query("INSERT INTO users VALUES('admin','admin','hash','admin')")
            .execute(&pool)
            .await?;
        let request = SimpleImportRequest {
            scan: ScanRequest {
                name: "untrusted-title.iso".into(),
                identity: "confirmed".into(),
                copy_complete: true,
            },
            accept_preserve: true,
        };
        let scan = enqueue(&pool, "admin", &request).await?.unwrap();
        let attempt = uuid::Uuid::new_v4().to_string();
        sqlx::query("UPDATE scan_jobs SET state='needs_review',attempt_id=?,sha256=?,size_bytes=100,report=? WHERE id=?")
            .bind(attempt).bind("a".repeat(64)).bind(inventory().to_string()).bind(&scan.id)
            .execute(&pool).await?;
        Ok((directory, pool, scan, request))
    }

    async fn inspected(pool: &SqlitePool) -> Result<String> {
        advance(pool).await?;
        let row = sqlx::query("SELECT * FROM scan_jobs")
            .fetch_one(pool)
            .await?;
        let probe: String = row.try_get("simple_probe_id")?;
        let report = serde_json::json!({
            "sha256":"a".repeat(64),"size_bytes":100,
            "staged_attempt_id":row.try_get::<String,_>("attempt_id")?,
            "inspection":{"title":1,"angle":1,"source_size_bytes":100,
                "warnings":["Title numbering is not proof of the movie."],
                "metadata":{"format":{"format_name":"dvdvideo","duration":"60"},
                    "streams":[{"codec_type":"video"}]}}
        });
        sqlx::query("UPDATE probe_jobs SET state='needs_review',report=? WHERE id=?")
            .bind(report.to_string())
            .bind(&probe)
            .execute(pool)
            .await?;
        Ok(probe)
    }

    #[tokio::test]
    async fn simple_chain_is_restart_safe_idempotent_and_stops_before_publication() -> Result<()> {
        let (directory, pool, scan, request) = fixture().await?;
        let (first, second) = tokio::join!(
            enqueue(&pool, "admin", &request),
            enqueue(&pool, "admin", &request)
        );
        assert_eq!(first?.unwrap().id, scan.id);
        assert_eq!(second?.unwrap().id, scan.id);
        let probe = inspected(&pool).await?;
        assert!(
            super::super::scans::inspect(
                &pool,
                "admin",
                &scan.id,
                &InspectRequest { title: 2, angle: 1 }
            )
            .await
            .is_err()
        );
        assert!(
            super::super::conversions::enqueue(
                &pool,
                "admin",
                &ConversionRequest {
                    probe_job_id: probe.clone(),
                    video_treatment: VideoTreatment::Yadif,
                }
            )
            .await?
            .is_none()
        );
        pool.close().await;
        let pool = super::super::db::connect(directory.path()).await?;
        let (first, second) = tokio::join!(advance(&pool), advance(&pool));
        first?;
        second?;
        let conversion = super::super::conversions::list(&pool).await?.remove(0);
        assert_eq!(conversion.video_treatment, VideoTreatment::Preserve);
        assert_eq!(conversion.probe_job_id, probe);
        validate_conversion(&pool, &conversion.id).await?;
        assert!(advanced(&pool, &scan.id).await?.is_none());
        advance(&pool).await?;
        assert_eq!(super::super::conversions::list(&pool).await?.len(), 1);
        sqlx::query("UPDATE conversion_jobs SET state='needs_review',movie_sha256=?")
            .bind("b".repeat(64))
            .execute(&pool)
            .await?;
        advance(&pool).await?;
        assert_eq!(
            super::super::scans::list(&pool).await?[0]
                .simple_state
                .as_deref(),
            Some("review")
        );
        assert_eq!(
            super::super::conversions::list(&pool).await?[0].state,
            wetube_core::ConversionState::NeedsReview
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM movies")
                .fetch_one(&pool)
                .await?,
            0
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM conversion_jobs WHERE publication IS NOT NULL"
            )
            .fetch_one(&pool)
            .await?,
            0
        );
        pool.close().await;
        Ok(())
    }

    #[tokio::test]
    async fn ambiguous_scan_retains_results_for_advanced_without_duplicate_work() -> Result<()> {
        let (_directory, pool, scan, request) = fixture().await?;
        let mut report = inventory();
        report["titles"][1]["duration_seconds"] = serde_json::json!(60);
        sqlx::query("UPDATE scan_jobs SET report=?")
            .bind(report.to_string())
            .execute(&pool)
            .await?;
        advance(&pool).await?;
        let stopped = super::super::scans::list(&pool).await?.remove(0);
        assert_eq!(stopped.simple_state.as_deref(), Some("advanced"));
        assert!(
            stopped
                .simple_message
                .unwrap()
                .contains("not uniquely longest")
        );
        assert!(super::super::probe_jobs::list(&pool).await?.is_empty());
        assert_eq!(
            enqueue(&pool, "admin", &request).await?.unwrap().id,
            scan.id
        );
        advance(&pool).await?;
        let page = super::super::import_history::page(&pool, "admin", false, None, 50).await?;
        assert_eq!(
            page.total, 1,
            "ambiguity remains actionable, not hidden in History"
        );
        let probe = super::super::scans::inspect(
            &pool,
            "admin",
            &scan.id,
            &InspectRequest { title: 2, angle: 1 },
        )
        .await?
        .unwrap();
        assert_eq!(probe.attempt_id, stopped.attempt_id);
        assert_eq!(probe.report.unwrap()["sha256"], "a".repeat(64));
        advance(&pool).await?;
        assert!(super::super::conversions::list(&pool).await?.is_empty());
        assert_eq!(super::super::probe_jobs::list(&pool).await?.len(), 1);
        pool.close().await;
        Ok(())
    }

    #[tokio::test]
    async fn failed_mismatched_and_revoked_inspections_never_autoconvert() -> Result<()> {
        for mutation in [
            "UPDATE probe_jobs SET state='failed',error='interrupted'",
            "UPDATE probe_jobs SET error='inspection incomplete'",
            "UPDATE probe_jobs SET title=2",
            "UPDATE probe_jobs SET source_identity='changed'",
            "UPDATE scan_jobs SET state='failed'",
            "UPDATE users SET role='viewer'",
            "UPDATE probe_jobs SET report=json_set(report,'$.inspection.metadata.format.duration','-1')",
            "UPDATE probe_jobs SET report=json_set(report,'$.inspection.metadata.format.duration','500')",
            "UPDATE probe_jobs SET report=json_set(report,'$.sha256','changed')",
            "UPDATE probe_jobs SET report=json_set(report,'$.inspection.angle',2)",
            "UPDATE probe_jobs SET report='{}'",
        ] {
            let (_directory, pool, _scan, _request) = fixture().await?;
            inspected(&pool).await?;
            sqlx::query(mutation).execute(&pool).await?;
            advance(&pool).await?;
            advance(&pool).await?;
            assert!(
                super::super::conversions::list(&pool).await?.is_empty(),
                "{mutation}"
            );
            assert_eq!(
                super::super::scans::list(&pool).await?[0]
                    .simple_state
                    .as_deref(),
                Some("advanced"),
                "{mutation}"
            );
            pool.close().await;
        }
        Ok(())
    }

    #[tokio::test]
    async fn advanced_handoff_and_claim_recheck_prevent_automatic_restart_or_bypass() -> Result<()>
    {
        let (_directory, pool, scan, request) = fixture().await?;
        let probe = inspected(&pool).await?;
        advanced(&pool, &scan.id).await?.unwrap();
        advance(&pool).await?;
        assert!(super::super::conversions::list(&pool).await?.is_empty());
        enqueue(&pool, "admin", &request).await?;
        advance(&pool).await?;
        assert!(super::super::conversions::list(&pool).await?.is_empty());
        assert!(
            super::super::conversions::enqueue(
                &pool,
                "admin",
                &ConversionRequest {
                    probe_job_id: probe,
                    video_treatment: VideoTreatment::Yadif,
                }
            )
            .await?
            .is_some(),
            "Advanced retains manual treatment choices"
        );
        pool.close().await;

        let (_directory, pool, _scan, _request) = fixture().await?;
        inspected(&pool).await?;
        advance(&pool).await?;
        let conversion = super::super::conversions::list(&pool).await?.remove(0);
        sqlx::query("UPDATE conversion_jobs SET video_treatment='yadif'")
            .execute(&pool)
            .await?;
        assert!(validate_conversion(&pool, &conversion.id).await.is_err());
        sqlx::query("UPDATE conversion_jobs SET video_treatment='preserve'")
            .execute(&pool)
            .await?;
        sqlx::query("UPDATE probe_jobs SET source_identity='changed'")
            .execute(&pool)
            .await?;
        assert!(validate_conversion(&pool, &conversion.id).await.is_err());
        advance(&pool).await?;
        assert!(validate_conversion(&pool, &conversion.id).await.is_err());
        sqlx::query("UPDATE conversion_jobs SET state='failed'")
            .execute(&pool)
            .await?;
        advance(&pool).await?;
        assert_eq!(
            super::super::conversions::list(&pool).await?.len(),
            1,
            "failed conversions are never retried automatically"
        );
        pool.close().await;
        Ok(())
    }

    #[tokio::test]
    async fn simple_reuses_an_unconsumed_scan_but_requires_both_acknowledgments() -> Result<()> {
        let (_directory, pool, scan, mut request) = fixture().await?;
        sqlx::query("UPDATE scan_jobs SET simple_state=NULL,simple_requested_by=NULL")
            .execute(&pool)
            .await?;
        request.accept_preserve = false;
        assert!(enqueue(&pool, "admin", &request).await.is_err());
        request.accept_preserve = true;
        request.scan.copy_complete = false;
        assert!(enqueue(&pool, "admin", &request).await.is_err());
        request.scan.copy_complete = true;
        let reused = enqueue(&pool, "admin", &request).await?.unwrap();
        assert_eq!(reused.id, scan.id);
        assert!(reused.report.is_some());
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM scan_jobs")
                .fetch_one(&pool)
                .await?,
            1
        );
        advance(&pool).await?;
        assert_eq!(super::super::probe_jobs::list(&pool).await?.len(), 1);
        pool.close().await;
        Ok(())
    }

    #[tokio::test]
    async fn simple_reused_scan_excludes_all_reviewed_siblings_until_advanced_handoff() -> Result<()>
    {
        for continue_simple in [false, true] {
            let (_directory, pool, scan, request) = fixture().await?;
            let first = inspected(&pool).await?;
            sqlx::query("UPDATE scan_jobs SET simple_state=NULL,simple_probe_id=NULL,simple_requested_by=NULL")
                .execute(&pool).await?;
            let sibling = super::super::scans::inspect(
                &pool,
                "admin",
                &scan.id,
                &InspectRequest { title: 2, angle: 1 },
            )
            .await?
            .unwrap();
            sqlx::query("UPDATE probe_jobs SET state='needs_review' WHERE id=?")
                .bind(&sibling.id)
                .execute(&pool)
                .await?;
            let reused = enqueue(&pool, "admin", &request).await?.unwrap();
            assert_eq!(reused.id, scan.id);
            assert_eq!(reused.simple_state.as_deref(), Some("inspection"));
            assert!(
                super::super::scans::inspect(
                    &pool,
                    "admin",
                    &scan.id,
                    &InspectRequest { title: 2, angle: 1 }
                )
                .await
                .is_err()
            );
            let manual = ConversionRequest {
                probe_job_id: sibling.id.clone(),
                video_treatment: VideoTreatment::Yadif,
            };
            assert!(
                super::super::conversions::enqueue(&pool, "admin", &manual)
                    .await?
                    .is_none()
            );
            if continue_simple {
                let (automatic, manual) = tokio::join!(
                    advance(&pool),
                    super::super::conversions::enqueue(&pool, "admin", &manual)
                );
                automatic?;
                assert!(
                    manual?.is_none(),
                    "the writer transaction excludes sibling conversions even during continuation"
                );
                advance(&pool).await?;
                let conversions = super::super::conversions::list(&pool).await?;
                assert_eq!(conversions.len(), 1);
                assert_eq!(conversions[0].probe_job_id, first);
                assert_eq!(conversions[0].video_treatment, VideoTreatment::Preserve);
            } else {
                advanced(&pool, &scan.id).await?.unwrap();
                let job = super::super::conversions::enqueue(&pool, "admin", &manual)
                    .await?
                    .unwrap();
                assert_eq!(job.probe_job_id, sibling.id);
                assert_eq!(job.video_treatment, VideoTreatment::Yadif);
                advance(&pool).await?;
                assert_eq!(super::super::conversions::list(&pool).await?.len(), 1);
            }
            pool.close().await;
        }
        Ok(())
    }

    #[tokio::test]
    async fn simple_reuses_verified_inspection_but_leaves_active_manual_selection_alone()
    -> Result<()> {
        for active in [false, true] {
            let (_directory, pool, scan, request) = fixture().await?;
            let probe = inspected(&pool).await?;
            sqlx::query("UPDATE scan_jobs SET simple_state=NULL,simple_probe_id=NULL,simple_requested_by=NULL")
                .execute(&pool).await?;
            if active {
                sqlx::query("UPDATE probe_jobs SET state='probing'")
                    .execute(&pool)
                    .await?;
            }
            let reused = enqueue(&pool, "admin", &request).await?.unwrap();
            assert_eq!(reused.id, scan.id);
            assert_eq!(
                reused.simple_state.as_deref(),
                Some(if active { "advanced" } else { "inspection" })
            );
            advance(&pool).await?;
            assert_eq!(super::super::probe_jobs::list(&pool).await?.len(), 1);
            let conversions = super::super::conversions::list(&pool).await?;
            if active {
                assert!(conversions.is_empty());
            } else {
                assert_eq!(conversions.len(), 1);
                assert_eq!(conversions[0].probe_job_id, probe);
            }
            pool.close().await;
        }
        Ok(())
    }
}
