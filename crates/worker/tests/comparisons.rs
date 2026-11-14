use anyhow::{Context, Result, ensure};
use std::{fs::OpenOptions, io::Write, process::Stdio, sync::Arc, time::Duration};
use tokio::process::Command;
use wetube_backend::{comparisons, incoming::identity, probe_jobs};
use wetube_worker::comparisons::tick;
const CLIP_CAP: u64 = 32 * 1024 * 1024;

async fn fixture_command(command: &mut Command) -> Result<()> {
    let status = tokio::time::timeout(
        Duration::from_secs(60),
        command
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .status(),
    )
    .await??;
    ensure!(
        status.success(),
        "authored DVD fixture command failed: {status}"
    );
    Ok(())
}

#[tokio::test]
async fn serialized_worker_prioritizes_all_queued_comparisons_before_conversion() -> Result<()> {
    use wetube_backend::{conversions, db};
    use wetube_core::{ConversionRequest, VideoTreatment};
    let root = tempfile::tempdir()?;
    let state = root.path().join("state");
    let incoming = root.path().join("incoming");
    let scratch = root.path().join("scratch");
    let media = root.path().join("media");
    for path in [&state, &incoming, &scratch, &media] {
        std::fs::create_dir(path)?;
    }
    db::initialize(&state).await?;
    let pool = db::connect(&state).await?;
    sqlx::query("INSERT INTO users VALUES ('admin','admin','hash','admin')")
        .execute(&pool)
        .await?;
    // Missing staged checksums deliberately fail immediately after each real claim.
    for id in ["full", "preview-one", "preview-two"] {
        sqlx::query("INSERT INTO probe_jobs(id,requested_by,name,source_identity,title,angle,state,created_at,attempt_id,report)
            VALUES (?,'admin','unused.iso','bound',1,1,'needs_review',1,?,'{}')")
            .bind(id).bind(uuid::Uuid::new_v4().to_string()).execute(&pool).await?;
    }
    let conversion = conversions::enqueue(
        &pool,
        "admin",
        &ConversionRequest {
            probe_job_id: "full".into(),
            video_treatment: VideoTreatment::Preserve,
        },
    )
    .await?
    .context("conversion not queued")?;
    let first = comparisons::enqueue(&pool, "admin", "preview-one")
        .await?
        .context("first comparison not queued")?;
    let second = comparisons::enqueue(&pool, "admin", "preview-two")
        .await?
        .context("second comparison not queued")?;
    sqlx::query("CREATE TABLE processing_order (kind TEXT,job_id TEXT)")
        .execute(&pool)
        .await?;
    sqlx::query(
        "CREATE TRIGGER record_comparison_claim AFTER UPDATE OF state ON comparison_jobs
        WHEN OLD.state='queued' AND NEW.state='running'
        BEGIN INSERT INTO processing_order VALUES ('comparison',NEW.id); END",
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "CREATE TRIGGER record_conversion_claim AFTER UPDATE OF state ON conversion_jobs
        WHEN OLD.state='queued' AND NEW.state='converting'
        BEGIN INSERT INTO processing_order VALUES ('conversion',NEW.id); END",
    )
    .execute(&pool)
    .await?;
    let expected = [
        ("comparison".to_owned(), first.id),
        ("comparison".to_owned(), second.id),
        ("conversion".to_owned(), conversion.id),
    ];
    for completed in 1..=expected.len() {
        let status = tokio::time::timeout(
            Duration::from_secs(20),
            Command::new(env!("CARGO_BIN_EXE_wetube-worker"))
                .arg("--state-dir")
                .arg(&state)
                .args(["run", "--once", "--incoming-dir"])
                .arg(&incoming)
                .arg("--scratch-dir")
                .arg(&scratch)
                .arg("--media-dir")
                .arg(&media)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .kill_on_drop(true)
                .status(),
        )
        .await??;
        ensure!(
            status.success(),
            "isolated worker iteration failed: {status}"
        );
        let actual: Vec<(String, String)> =
            sqlx::query_as("SELECT kind,job_id FROM processing_order ORDER BY rowid")
                .fetch_all(&pool)
                .await?;
        assert_eq!(actual, expected[..completed]);
    }
    pool.close().await;
    Ok(())
}

#[tokio::test]
async fn dvd_midpoint_and_short_comparisons_reuse_input_validate_both_and_retain_failures()
-> Result<()> {
    use wetube_backend::{db, incoming::IncomingRoot};
    use wetube_core::{JobState, ProbeRequest};
    let root = tempfile::tempdir()?;
    let state = root.path().join("state");
    let incoming = root.path().join("incoming");
    let scratch = root.path().join("scratch");
    std::fs::create_dir(&state)?;
    std::fs::create_dir(&incoming)?;
    for (name, seconds) in [("long", "20"), ("short", "3")] {
        fixture_command(
            Command::new("ffmpeg")
                .args([
                    "-hide_banner",
                    "-nostdin",
                    "-v",
                    "error",
                    "-f",
                    "lavfi",
                    "-i",
                    "testsrc2=size=720x480:rate=30000/1001",
                    "-f",
                    "lavfi",
                    "-i",
                    "sine=frequency=440:sample_rate=48000",
                    "-t",
                    seconds,
                    "-target",
                    "ntsc-dvd",
                    "-aspect",
                    "4:3",
                    "-threads",
                    "2",
                    "-c:a",
                    "ac3",
                    "-b:a",
                    "192k",
                    "-ac",
                    "2",
                ])
                .arg(root.path().join(format!("{name}.vob"))),
        )
        .await?;
    }
    let xml = root.path().join("dvd.xml");
    std::fs::write(
        &xml,
        format!(
            "<dvdauthor><vmgm/><titleset><titles><video format=\"ntsc\" aspect=\"4:3\"/><audio format=\"ac3\" lang=\"en\"/><pgc><vob file=\"{}\"/></pgc><pgc><vob file=\"{}\"/></pgc></titles></titleset></dvdauthor>",
            root.path().join("long.vob").display(),
            root.path().join("short.vob").display()
        ),
    )?;
    let dvd = root.path().join("dvd");
    fixture_command(
        Command::new("dvdauthor")
            .env("VIDEO_FORMAT", "NTSC")
            .arg("-o")
            .arg(&dvd)
            .arg("-x")
            .arg(xml),
    )
    .await?;
    let iso = incoming.join("fixture.iso");
    fixture_command(
        Command::new("genisoimage")
            .args(["-quiet", "-dvd-video", "-o"])
            .arg(&iso)
            .arg(dvd),
    )
    .await?;
    db::initialize(&state).await?;
    let pool = db::connect(&state).await?;
    sqlx::query("INSERT INTO users VALUES ('admin','admin','hash','admin')")
        .execute(&pool)
        .await?;
    let config = Arc::new(wetube_worker::imports::ImportConfig::open(
        &incoming,
        &scratch,
        128 * 1024 * 1024,
        1024 * 1024 * 1024,
        Duration::from_secs(30),
    )?);
    let limits = wetube_worker::conversions::Limits {
        timeout: Duration::from_secs(60),
        threads: 2,
        max_output_bytes: CLIP_CAP,
    };
    // Confirm only after the final write handle has closed on DrvFS.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let original = IncomingRoot::open(&incoming)?.list()?.remove(0);
    let mut last_probe = None;
    for title in [1, 2] {
        let probe = probe_jobs::enqueue(
            &pool,
            "admin",
            &ProbeRequest {
                name: original.name.clone(),
                identity: original.identity.clone(),
                title,
                angle: 1,
                copy_complete: true,
            },
        )
        .await?
        .context("probe not queued")?;
        wetube_worker::imports::tick(&pool, config.clone()).await?;
        let probe = probe_jobs::list(&pool)
            .await?
            .into_iter()
            .find(|p| p.id == probe.id)
            .unwrap();
        ensure!(
            probe.state == wetube_core::ProbeState::NeedsReview,
            "inspection failed: {:?}",
            probe.error
        );
        let staged_path = scratch
            .join(probe.attempt_id.as_ref().unwrap())
            .join("input.iso");
        let before = identity(&std::fs::metadata(&staged_path)?);
        let job = comparisons::enqueue(&pool, "admin", &probe.id)
            .await?
            .context("comparison not queued")?;
        assert!(
            comparisons::enqueue(&pool, "admin", &probe.id)
                .await?
                .is_none()
        );
        tick(&pool, config.clone(), &limits).await?;
        let job = comparisons::latest(&pool, &probe.id)
            .await?
            .filter(|j| j.id == job.id)
            .unwrap();
        let report = job.report.as_ref().context("missing comparison report")?;
        ensure!(
            job.state == JobState::Succeeded,
            "comparison failed: {:?}\n{report}",
            job.error
        );
        if title == 1 {
            assert!(report["requested_start_seconds"].as_f64().unwrap() >= 4.0);
            assert_eq!(report["requested_duration_seconds"], 10.0);
            assert!(
                report["seek"]["native_dvd_seek"]
                    .as_str()
                    .unwrap()
                    .contains("seeking:")
            );
        } else {
            assert_eq!(report["requested_start_seconds"], 0.0);
            assert!(report["requested_duration_seconds"].as_f64().unwrap() < 4.0);
        }
        assert_eq!(
            report["preserve"]["decoded_frames"],
            report["yadif"]["decoded_frames"]
        );
        assert_eq!(report["yadif"]["filter"], wetube_core::YADIF_FILTER);
        assert!(report["preserve"]["filter"].is_null());
        assert_eq!(identity(&std::fs::metadata(staged_path)?), before);
        assert!(
            !scratch
                .join(job.attempt_id.as_ref().unwrap())
                .join("input.iso")
                .exists(),
            "no second ISO copy"
        );
        assert!(
            comparisons::enqueue(&pool, "admin", &probe.id)
                .await?
                .is_none()
        );
        last_probe = Some((probe, job));
    }
    let (probe, job) = last_probe.unwrap();
    let retained = scratch
        .join(job.attempt_id.as_ref().unwrap())
        .join("preserve.mp4");
    sqlx::query("UPDATE comparison_jobs SET state='running' WHERE id=?")
        .bind(&job.id)
        .execute(&pool)
        .await?;
    wetube_worker::comparisons::recover(&pool).await?;
    assert!(retained.is_file());
    assert_eq!(
        comparisons::latest(&pool, &probe.id).await?.unwrap().state,
        JobState::Failed
    );
    let failed = comparisons::enqueue(&pool, "admin", &probe.id)
        .await?
        .unwrap();
    let path = scratch
        .join(probe.attempt_id.as_ref().unwrap())
        .join("input.iso");
    let mut file = OpenOptions::new().write(true).open(&path)?;
    file.write_all(b"corrupt")?;
    file.sync_all()?;
    drop(file);
    tick(&pool, config.clone(), &limits).await?;
    let failed = comparisons::latest(&pool, &probe.id)
        .await?
        .filter(|j| j.id == failed.id)
        .unwrap();
    assert_eq!(failed.state, JobState::Failed);
    assert!(failed.error.as_ref().unwrap().contains("checksum"));
    assert!(
        scratch
            .join(failed.attempt_id.unwrap())
            .join("comparison.json")
            .is_file()
    );
    assert_eq!(
        IncomingRoot::open(&incoming)?.list()?.remove(0).identity,
        original.identity
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM movies")
            .fetch_one(&pool)
            .await?,
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM conversion_jobs")
            .fetch_one(&pool)
            .await?,
        0
    );
    pool.close().await;
    Ok(())
}
