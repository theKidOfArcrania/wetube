use std::process::Command;

fn fixture_command(command: &mut Command) -> anyhow::Result<()> {
    let output = command.output()?;
    anyhow::ensure!(
        output.status.success(),
        "DVD fixture command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

#[tokio::test]
#[ignore = "integration acceptance requires stable C-backed ctime; strict input identity checks remain enabled"]
async fn simple_authored_dvd_runs_real_chain_once_and_requires_publication_review()
-> anyhow::Result<()> {
    use anyhow::Context;
    use wetube_backend::{db, incoming::IncomingRoot, simple_imports};
    use wetube_core::{ConversionState, ScanRequest, SimpleImportRequest};
    let root = tempfile::tempdir()?;
    let state = root.path().join("state");
    let incoming = root.path().join("incoming");
    let scratch = root.path().join("scratch");
    let media = root.path().join("media");
    for path in [&state, &incoming, &scratch, &media] {
        std::fs::create_dir(path)?;
    }
    for (name, seconds) in [("feature", "3"), ("extra", "1")] {
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
        )?;
    }
    let xml = root.path().join("dvd.xml");
    std::fs::write(
        &xml,
        format!(
            "<dvdauthor><vmgm/><titleset><titles><video format=\"ntsc\" aspect=\"4:3\"/><audio format=\"ac3\" lang=\"en\"/><pgc><vob file=\"{}\"/></pgc><pgc><vob file=\"{}\"/></pgc></titles></titleset></dvdauthor>",
            root.path().join("feature.vob").display(),
            root.path().join("extra.vob").display()
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
    )?;
    fixture_command(
        Command::new("genisoimage")
            .args(["-quiet", "-dvd-video", "-o"])
            .arg(incoming.join("fixture.iso"))
            .arg(dvd),
    )?;
    db::initialize(&state).await?;
    let pool = db::connect(&state).await?;
    sqlx::query("INSERT INTO users VALUES('admin','admin','hash','admin')")
        .execute(&pool)
        .await?;
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let source = IncomingRoot::open(&incoming)?;
    let (handle, original) = source.open_file("fixture.iso")?;
    let request = SimpleImportRequest {
        scan: ScanRequest {
            name: original.name.clone(),
            identity: original.identity.clone(),
            copy_complete: true,
        },
        accept_preserve: true,
    };
    let queued = simple_imports::enqueue(&pool, "admin", &request)
        .await?
        .context("Simple import not queued")?;
    for _ in 0..2 {
        fixture_command(
            Command::new(env!("CARGO_BIN_EXE_wetube-worker"))
                .arg("--state-dir")
                .arg(&state)
                .args(["run", "--once", "--incoming-dir"])
                .arg(&incoming)
                .arg("--scratch-dir")
                .arg(&scratch)
                .arg("--media-dir")
                .arg(&media)
                .args([
                    "--max-iso-bytes",
                    "33554432",
                    "--max-scratch-bytes",
                    "134217728",
                    "--max-output-bytes",
                    "8388608",
                    "--tool-timeout-secs",
                    "60",
                ]),
        )?;
    }
    let scan = wetube_backend::scans::list(&pool).await?.remove(0);
    let probes = wetube_backend::probe_jobs::list(&pool).await?;
    let conversions = wetube_backend::conversions::list(&pool).await?;
    assert_eq!(scan.id, queued.id);
    assert_eq!(
        scan.state,
        wetube_core::ScanState::NeedsReview,
        "scan error: {:?}",
        scan.error
    );
    assert_eq!(probes.len(), 1);
    assert_eq!((probes[0].title, probes[0].angle), (1, 1));
    assert_eq!(
        probes[0].attempt_id, scan.attempt_id,
        "reuse the real scan archive"
    );
    assert_eq!(
        conversions.len(),
        1,
        "restart must not enqueue a second conversion"
    );
    assert_eq!(
        conversions[0].state,
        ConversionState::NeedsReview,
        "conversion error: {:?}; report: {:?}",
        conversions[0].error,
        conversions[0].report
    );
    assert_eq!(
        conversions[0].video_treatment,
        wetube_core::VideoTreatment::Preserve
    );
    assert_eq!(scan.simple_state.as_deref(), Some("review"));
    assert_eq!(
        simple_imports::enqueue(&pool, "admin", &request)
            .await?
            .unwrap()
            .id,
        queued.id
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
    assert_eq!(std::fs::read_dir(&media)?.count(), 0);
    source.verify(&original.name, &handle, &original.identity)?;
    pool.close().await;
    Ok(())
}

#[tokio::test]
async fn integrated_scan_failure_retains_staged_input_logs_and_no_movies() -> anyhow::Result<()> {
    for reject_progress in [false, true] {
        integrated_scan_failure_case(reject_progress).await?;
    }
    Ok(())
}

async fn integrated_scan_failure_case(reject_progress: bool) -> anyhow::Result<()> {
    use anyhow::{Context, ensure};
    use wetube_backend::{db, incoming::IncomingRoot, scans};
    use wetube_core::{ScanRequest, ScanState};
    let root = tempfile::tempdir()?;
    let state = root.path().join("state");
    let incoming = root.path().join("incoming");
    let scratch = root.path().join("scratch");
    std::fs::create_dir(&state)?;
    std::fs::create_dir(&incoming)?;
    let contents = b"completed fixture, not a DVD";
    std::fs::write(incoming.join("invalid.iso"), contents)?;
    std::fs::File::open(incoming.join("invalid.iso"))?.sync_all()?;
    assert_eq!(std::fs::read(incoming.join("invalid.iso"))?, contents);
    // Let DrvFS finalize newly-created fixture metadata before confirming identity.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    db::initialize(&state).await?;
    let pool = db::connect(&state).await?;
    sqlx::query("INSERT INTO users VALUES('admin','admin','hash','admin')")
        .execute(&pool)
        .await?;
    if reject_progress {
        sqlx::query("CREATE TRIGGER reject_scan_progress BEFORE UPDATE OF progress ON scan_jobs BEGIN SELECT RAISE(FAIL,'injected telemetry failure'); END").execute(&pool).await?;
    }
    let file = IncomingRoot::open(&incoming)?.list()?.remove(0);
    let job = scans::enqueue(
        &pool,
        "admin",
        &ScanRequest {
            name: file.name,
            identity: file.identity,
            copy_complete: true,
        },
    )
    .await?
    .context("scan not queued")?;
    let output = Command::new(env!("CARGO_BIN_EXE_wetube-worker"))
        .arg("--state-dir")
        .arg(&state)
        .args(["run", "--once", "--incoming-dir"])
        .arg(&incoming)
        .arg("--scratch-dir")
        .arg(&scratch)
        .output()?;
    ensure!(
        output.status.success(),
        "worker startup failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let failed = scans::list(&pool).await?.remove(0);
    assert_eq!(failed.id, job.id);
    assert_eq!(failed.state, ScanState::Failed);
    if reject_progress {
        assert!(failed.progress.is_none());
    } else {
        let progress = failed
            .progress
            .as_ref()
            .context("missing retained scan progress")?;
        assert_eq!(progress.workflow, wetube_core::MediaWorkflow::Scan);
        assert_eq!(progress.stage, 2);
        assert_eq!(progress.total_units, None);
    }
    assert!(
        failed
            .error
            .as_ref()
            .is_some_and(|error| error.contains("DVD discovery"))
    );
    let attempt = scratch.join(failed.attempt_id.context("missing retained attempt")?);
    assert_eq!(
        std::fs::read(attempt.join("input.iso"))
            .with_context(|| format!("scan error: {:?}", failed.error))?,
        contents
    );
    assert_eq!(std::fs::read(incoming.join("invalid.iso"))?, contents);
    assert!(attempt.join("scan.stderr.txt").is_file());
    assert!(attempt.join("scan.stdout.txt").is_file());
    assert!(
        failed.report.is_none(),
        "invalid ISO never yields invented title choices"
    );
    let movies: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM movies")
        .fetch_one(&pool)
        .await?;
    assert_eq!(movies, 0);
    pool.close().await;
    Ok(())
}

#[tokio::test]
#[ignore = "requires an explicitly confirmed real DVD copy and retains scratch output"]
async fn real_dvd_copy_reaches_review_without_changing_incoming() -> anyhow::Result<()> {
    use anyhow::{Context, ensure};
    use std::{fs::OpenOptions, io::Write, os::unix::fs::OpenOptionsExt, path::PathBuf};
    use wetube_backend::{db, incoming::IncomingRoot, probe_jobs};
    use wetube_core::{ProbeRequest, ProbeState};

    ensure!(
        std::env::var("WETUBE_TEST_COPY_COMPLETE").as_deref() == Ok("1"),
        "set WETUBE_TEST_COPY_COMPLETE=1 only for a finished disposable copy with writers closed"
    );
    let iso = PathBuf::from(std::env::var("WETUBE_TEST_ISO").context("set WETUBE_TEST_ISO")?);
    let scratch =
        PathBuf::from(std::env::var("WETUBE_TEST_SCRATCH").context("set WETUBE_TEST_SCRATCH")?);
    let title: u8 = std::env::var("WETUBE_TEST_TITLE")
        .context("set WETUBE_TEST_TITLE explicitly")?
        .parse()?;
    let angle: u8 = std::env::var("WETUBE_TEST_ANGLE")
        .unwrap_or_else(|_| "1".into())
        .parse()?;
    let staging_timeout =
        std::env::var("WETUBE_TEST_STAGING_TIMEOUT_SECS").unwrap_or_else(|_| "600".into());
    ensure!(
        (1..=99).contains(&title) && (1..=9).contains(&angle),
        "invalid DVD title/angle"
    );
    let incoming = iso.parent().context("ISO needs a parent directory")?;
    let name = iso
        .file_name()
        .and_then(|name| name.to_str())
        .context("ISO needs a UTF-8 filename")?;
    let root = IncomingRoot::open(incoming)?;
    let (source, original) = root.open_file(name)?;
    let state = tempfile::tempdir()?;
    db::initialize(state.path()).await?;
    let pool = db::connect(state.path()).await?;
    sqlx::query(
        "INSERT INTO users VALUES ('integration', 'integration', 'unusable test hash', 'admin')",
    )
    .execute(&pool)
    .await?;
    let queued = probe_jobs::enqueue(
        &pool,
        "integration",
        &ProbeRequest {
            name: name.into(),
            identity: original.identity.clone(),
            title,
            angle,
            copy_complete: true,
        },
    )
    .await?
    .context("expected an empty isolated queue")?;

    let started = std::time::Instant::now();
    let output = Command::new(env!("CARGO_BIN_EXE_wetube-worker"))
        .arg("--state-dir")
        .arg(state.path())
        .args(["run", "--once", "--incoming-dir"])
        .arg(incoming)
        .arg("--scratch-dir")
        .arg(&scratch)
        .arg("--staging-timeout-secs")
        .arg(staging_timeout)
        .output()?;
    ensure!(
        output.status.success(),
        "worker failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let job = probe_jobs::list(&pool)
        .await?
        .into_iter()
        .find(|job| job.id == queued.id)
        .context("worker lost the queued job")?;
    let attempt = job
        .attempt_id
        .as_ref()
        .context("worker did not allocate an attempt")?;
    let directory = scratch.join(attempt);
    let report_path = directory.join("integration-report.json");
    if directory.is_dir() {
        let mut report = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&report_path)?;
        report.write_all(&serde_json::to_vec_pretty(&job)?)?;
        report.sync_all()?;
    }
    root.verify(name, &source, &original.identity)?;
    ensure!(
        job.state == ProbeState::NeedsReview,
        "inspection did not reach review: {:?}",
        job.error
    );
    let report = job.report.as_ref().context("missing inspection report")?;
    ensure!(
        report["size_bytes"].as_u64() == Some(original.size_bytes),
        "staged size mismatch"
    );
    ensure!(
        std::fs::metadata(directory.join("input.iso"))?.len() == original.size_bytes,
        "retained ISO size mismatch"
    );
    let digest = report["sha256"].as_str().context("missing staged digest")?;
    ensure!(
        digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "invalid digest"
    );
    ensure!(
        report["inspection"]["metadata"]["format"]["format_name"] == "dvdvideo",
        "missing DVD metadata"
    );
    ensure!(
        report["inspection"]["title"].as_u64() == Some(title as u64),
        "wrong inspected title"
    );
    let movies: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM movies")
        .fetch_one(&pool)
        .await?;
    ensure!(movies == 0, "inspection unexpectedly published media");
    println!(
        "Needs review after {:.1}s; {} bytes; SHA-256 {digest}; retained report: {}",
        started.elapsed().as_secs_f64(),
        original.size_bytes,
        report_path.display()
    );
    pool.close().await;
    Ok(())
}

#[test]
fn invalid_dvd_fails_without_emitting_a_report_or_changing_the_input() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("incomplete.iso");
    let contents = b"this is an incomplete DVD copy";
    std::fs::write(&path, contents).unwrap();
    std::fs::File::open(&path).unwrap().sync_all().unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), contents);
    std::thread::sleep(std::time::Duration::from_millis(500));
    let output = Command::new(env!("CARGO_BIN_EXE_wetube-worker"))
        .args(["inspect-dvd", "--title", "1", "--copy-complete", "--iso"])
        .arg(&path)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(
        error.contains("DVD inspection failed") || error.contains("run ffprobe DVD inspection"),
        "{error}"
    );
    assert_eq!(std::fs::read(path).unwrap(), contents);
}

#[tokio::test]
async fn configured_worker_persists_staged_failure_and_keeps_both_copies() {
    use wetube_backend::{db, incoming::IncomingRoot, probe_jobs};
    use wetube_core::{ProbeRequest, ProbeState};
    let state = tempfile::tempdir().unwrap();
    let incoming = tempfile::tempdir().unwrap();
    let scratch_root = tempfile::tempdir().unwrap();
    let scratch = scratch_root.path().join("scratch");
    let contents = b"completed but invalid DVD copy";
    std::fs::write(incoming.path().join("movie.iso"), contents).unwrap();
    std::fs::File::open(incoming.path().join("movie.iso"))
        .unwrap()
        .sync_all()
        .unwrap();
    assert_eq!(
        std::fs::read(incoming.path().join("movie.iso")).unwrap(),
        contents
    );
    // Let DrvFS finalize newly-created fixture metadata before confirming identity.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    db::initialize(state.path()).await.unwrap();
    let pool = db::connect(state.path()).await.unwrap();
    sqlx::query("INSERT INTO users VALUES ('admin', 'admin', 'test hash', 'admin')")
        .execute(&pool)
        .await
        .unwrap();
    let file = IncomingRoot::open(incoming.path())
        .unwrap()
        .list()
        .unwrap()
        .remove(0);
    probe_jobs::enqueue(
        &pool,
        "admin",
        &ProbeRequest {
            name: file.name,
            identity: file.identity,
            title: 1,
            angle: 1,
            copy_complete: true,
        },
    )
    .await
    .unwrap()
    .unwrap();
    let disabled = Command::new(env!("CARGO_BIN_EXE_wetube-worker"))
        .arg("--state-dir")
        .arg(state.path())
        .args(["run", "--once"])
        .output()
        .unwrap();
    assert!(disabled.status.success());
    assert_eq!(
        probe_jobs::list(&pool).await.unwrap()[0].state,
        ProbeState::Queued
    );
    assert!(!scratch.exists());
    let output = Command::new(env!("CARGO_BIN_EXE_wetube-worker"))
        .arg("--state-dir")
        .arg(state.path())
        .args(["run", "--once", "--incoming-dir"])
        .arg(incoming.path())
        .arg("--scratch-dir")
        .arg(&scratch)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let job = probe_jobs::list(&pool).await.unwrap().remove(0);
    assert_eq!(job.state, ProbeState::Failed);
    assert!(
        job.report.as_ref().unwrap_or_else(|| panic!(
            "expected verified staging before inspection failure: {:?}",
            job.error
        ))["sha256"]
            .is_string()
    );
    assert_eq!(
        std::fs::read(scratch.join(job.attempt_id.unwrap()).join("input.iso")).unwrap(),
        contents
    );
    assert_eq!(
        std::fs::read(incoming.path().join("movie.iso")).unwrap(),
        contents
    );
    pool.close().await;
}
