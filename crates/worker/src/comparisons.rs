//! Short, private comparisons. No catalogue or publication operations live here.
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use sqlx::{Row, SqlitePool};
use std::{
    fs::{File, OpenOptions},
    io::{Seek, SeekFrom, Write},
    os::{
        fd::AsRawFd,
        unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    },
    path::Path,
    process::Stdio,
    sync::Arc,
    time::Duration,
};
use tokio::process::Command;
use wetube_backend::{comparisons, incoming::identity, probe_jobs};
use wetube_core::MediaWorkflow;

use super::progress::{Progress, Recording};

const SOURCE_CAP: u64 = 256 * 1024 * 1024;
const CLIP_CAP: u64 = 32 * 1024 * 1024;
const LOG_CAP: usize = 2 * 1024 * 1024;

pub async fn recover(pool: &SqlitePool) -> Result<()> {
    sqlx::query("UPDATE comparison_jobs SET state='failed',error='Worker interrupted; partial comparison retained, unregistered. Retry explicitly.' WHERE state='running'
        AND NOT EXISTS(SELECT 1 FROM processing_queue WHERE job_id=comparison_jobs.id AND kind='comparison' AND state IN ('running','cancelling') AND finish_state IS NOT NULL)")
        .execute(pool).await?;
    Ok(())
}

fn window(metadata: &Value) -> Result<(f64, f64)> {
    let duration: f64 = metadata["format"]["duration"]
        .as_str()
        .context("inspection has no duration")?
        .parse()?;
    ensure!(
        duration.is_finite() && duration > 0.0 && duration <= 86400.0,
        "invalid selected program duration"
    );
    let length = duration.min(10.0);
    Ok(((duration - length) / 2.0, length))
}

fn input(command: &mut Command, file: &File) -> Result<()> {
    let mut file = file.try_clone()?;
    file.seek(SeekFrom::Start(0))?;
    command
        .args(["-i", "/proc/self/fd/0"])
        .stdin(Stdio::from(file));
    Ok(())
}

fn ffmpeg(threads: u8) -> Command {
    let mut command = Command::new("ffmpeg");
    command.env("DVDCSS_CACHE", "off").args([
        "-hide_banner",
        "-nostdin",
        "-nostats",
        "-v",
        "error",
        "-xerror",
        "-err_detect",
        "explode",
        "-max_error_rate",
        "0",
        "-threads",
        &threads.to_string(),
        "-filter_threads",
        "1",
    ]);
    command
}

fn create(path: &Path) -> Result<File> {
    Ok(OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?)
}

fn open(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    ensure!(
        file.metadata()?.is_file() && file.metadata()?.len() > 0,
        "empty/nonregular clip"
    );
    Ok(file)
}

fn verify_shared_source(file: &File, before: &std::fs::Metadata) -> Result<()> {
    let after = file.metadata()?;
    let describe = |metadata: &std::fs::Metadata| {
        format!(
            "dev={} ino={} size={} mtime={}.{:09} ctime={}.{:09}",
            metadata.dev(),
            metadata.ino(),
            metadata.len(),
            metadata.mtime(),
            metadata.mtime_nsec(),
            metadata.ctime(),
            metadata.ctime_nsec()
        )
    };
    ensure!(
        identity(before) == identity(&after),
        "shared source sample changed: before [{}]; after [{}]",
        describe(before),
        describe(&after)
    );
    Ok(())
}

fn output(command: &mut Command, file: &File, cap: u64, format: &str) -> Result<()> {
    ensure!(
        unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFD, 0) } != -1,
        "inherit bounded output descriptor"
    );
    command.args([
        "-y",
        "-fs",
        &cap.to_string(),
        "-f",
        format,
        &format!("/proc/self/fd/{}", file.as_raw_fd()),
    ]);
    // Unlike -fs (which may overshoot), this is a hard per-invocation file-size limit.
    unsafe {
        command.pre_exec(move || {
            let limit = libc::rlimit {
                rlim_cur: cap,
                rlim_max: cap,
            };
            if libc::setrlimit(libc::RLIMIT_FSIZE, &limit) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    Ok(())
}

async fn run(
    command: &mut Command,
    directory: &Path,
    phase: &str,
    timeout: Duration,
    seek: bool,
    progress: &Progress,
) -> Result<Value> {
    let stdout = create(&directory.join(format!("{phase}.stdout.txt")))?;
    let stderr = create(&directory.join(format!("{phase}.stderr.txt")))?;
    let result = super::process::run_logged_progress(
        command,
        timeout,
        LOG_CAP,
        stdout.try_clone()?,
        stderr.try_clone()?,
        Some(progress),
    )
    .await;
    stdout.sync_all()?;
    stderr.sync_all()?;
    let result = result.with_context(|| phase.to_owned())?;
    ensure!(
        result.status.success(),
        "{phase} failed: {}",
        String::from_utf8_lossy(&result.stderr)
            .chars()
            .take(2048)
            .collect::<String>()
    );
    if seek {
        // FFmpeg can continue from the beginning after an unsuccessful input -ss.
        // Require the dvdvideo demuxer's *successful* native NAV seek trace instead.
        let proof = seek_proof(&result.stderr)
            .context("DVD native seek was not proven; refusing a beginning-of-film fallback")?;
        return Ok(json!({"native_dvd_seek": proof}));
    }
    ensure!(
        result.stderr.is_empty(),
        "{phase} reported media errors: {}",
        String::from_utf8_lossy(&result.stderr)
            .chars()
            .take(2048)
            .collect::<String>()
    );
    Ok(json!(String::from_utf8(result.stdout)?))
}

fn seek_proof(bytes: &[u8]) -> Option<String> {
    std::str::from_utf8(bytes)
        .ok()?
        .lines()
        .find(|line| line.contains("seeking: requested_nav_pts=") && line.contains(" new_nav_pts="))
        .map(str::to_owned)
}

async fn metadata(file: &File, timeout: Duration) -> Result<Value> {
    let mut command = Command::new("ffprobe");
    command.args([
        "-v",
        "error",
        "-protocol_whitelist",
        "file,pipe",
        "-show_format",
        "-show_streams",
        "-count_packets",
        "-of",
        "json",
    ]);
    input(&mut command, file)?;
    let result = super::process::run(&mut command, timeout, LOG_CAP).await?;
    ensure!(
        result.status.success() && result.stderr.is_empty(),
        "clip metadata validation failed"
    );
    Ok(serde_json::from_slice(&result.stdout)?)
}

fn decoded_progress(bytes: &str) -> Result<(u64, f64)> {
    let mut frames = None;
    let mut seconds = None;
    let mut ended = false;
    for line in bytes.lines() {
        if let Some((key, value)) = line.split_once('=') {
            match key {
                "frame" => frames = value.trim().parse::<u64>().ok(),
                "out_time_us" => seconds = value.parse::<i64>().ok(),
                "progress" => ended = value == "end",
                _ => {}
            }
        }
    }
    let frames = frames.context("no decoded frames")?;
    let seconds = seconds.context("no decoded timestamp")? as f64 / 1_000_000.0;
    ensure!(
        ended && frames > 0 && seconds.is_finite() && seconds > 0.0,
        "incomplete clip decode"
    );
    Ok((frames, seconds))
}

async fn validate(
    file: &File,
    directory: &Path,
    phase: &str,
    timeout: Duration,
    progress: &Progress,
    stage: usize,
    duration: f64,
) -> Result<(Value, u64, f64)> {
    let before = identity(&file.metadata()?);
    progress.stage(stage, &format!("Reading {phase} metadata"), None, "");
    let metadata = metadata(file, timeout).await?;
    progress.stage(stage, phase, Some(duration), "media seconds");
    let mut command = ffmpeg(2);
    input(&mut command, file)?;
    command.args([
        "-map",
        "0:v:0",
        "-map",
        "0:a?",
        "-threads",
        "2",
        "-fps_mode",
        "passthrough",
        "-enc_time_base:v",
        "filter",
        "-progress",
        "pipe:1",
        "-f",
        "null",
        "-",
    ]);
    let result = run(&mut command, directory, phase, timeout, false, progress).await?;
    let (frames, seconds) = decoded_progress(result.as_str().context("missing decode progress")?)?;
    ensure!(
        identity(&file.metadata()?) == before,
        "clip changed during validation"
    );
    Ok((metadata, frames, seconds))
}

async fn generate(
    config: Arc<super::imports::ImportConfig>,
    probe: &wetube_core::ProbeJob,
    attempt: &str,
    limits: &super::conversions::Limits,
    progress: &Progress,
) -> Result<Value> {
    let directory = config.scratch_path().join(attempt);
    std::fs::DirBuilder::new().mode(0o700).create(&directory)?;
    File::open(config.scratch_path())?.sync_all()?;
    let mut report = json!({
        "version":1,"comparison_only":true,"published":false,"full_disc_validated":false,
        "attempt_id":attempt,"probe_job_id":probe.id,"title":probe.title,"angle":probe.angle,
        "profile":{"video_codec":"libx264","preset":"veryfast","crf":20,"pixel_format":"yuv420p",
            "audio_codec":"aac","audio_channels":2,"audio_bitrate":"192k",
            "shared_sample_audio_filter":"aresample=async=1:first_pts=0"},
        "timing":"Approximate DVD midpoint; no full-disc preindex. No motion detection.",
        "warnings":["H.264/AAC comparison is not lossless and does not validate the full title/disc.",
            "Preserve uses no interlace filter. Yadif uses the full-conversion filter; edge frames may differ from full-film context."]
    });
    let result: Result<()> = async {
        let cfg = config.clone();
        let selected = probe.clone();
        let revalidation = progress.clone();
        let staged = tokio::task::spawn_blocking(move || super::imports::revalidate_progress(&cfg, &selected, Some(&revalidation))).await??;
        report["input_sha256"] = json!(staged.sha256);
        report["input_identity"] = json!(staged.identity);
        report["input_size_bytes"] = json!(staged.size_bytes);
        report["staged_attempt_id"] = probe.report.as_ref().and_then(|r| r.get("staged_attempt_id"))
            .cloned().unwrap_or_else(|| json!(probe.attempt_id));
        let inspection = &probe.report.as_ref().context("missing inspection")?["inspection"];
        ensure!(inspection["title"].as_u64() == Some(u64::from(probe.title))
            && inspection["angle"].as_u64() == Some(u64::from(probe.angle)), "stale program selection");
        report["inspection"] = inspection.clone();
        let (start, length) = window(&inspection["metadata"])?;
        report["requested_start_seconds"] = json!(start);
        report["requested_duration_seconds"] = json!(length);
        let cap = limits.max_output_bytes.min(CLIP_CAP);
        config.require_capacity(SOURCE_CAP + 2 * cap + 32 * 1024 * 1024)?;
        let timeout = limits.timeout.min(Duration::from_secs(180));
        staged.verify()?;
        progress.stage(1, "Extracting shared midpoint sample", Some(length), "media seconds");
        let source = create(&directory.join("source.mkv"))?;
        let mut command = ffmpeg(limits.threads);
        command.args(["-f", "dvdvideo", "-title", &probe.title.to_string(),
            "-angle", &probe.angle.to_string(), "-preindex", "0"]);
        if start > 0.0 {
            command.args(["-v", "debug", "-ss", &format!("{start:.6}")]);
        }
        input(&mut command, &staged.file)?;
        command.args(["-t", &format!("{length:.6}"), "-map", "0:v:0", "-map", "0:a?",
            "-sn", "-dn", "-map_metadata", "-1", "-map_chapters", "-1", "-c:v", "ffv1",
            "-threads", &limits.threads.to_string(), "-fps_mode", "passthrough",
            "-enc_time_base:v", "filter", "-c:a", "pcm_s16le",
            "-af", "aresample=async=1:first_pts=0", "-progress", "pipe:1"]);
        output(&mut command, &source, SOURCE_CAP, "matroska")?;
        report["seek"] = run(&mut command, &directory, "extract", timeout, start > 0.0, progress).await?;
        progress.stage(1, "Syncing shared sample", None, "");
        source.sync_all()?;
        drop(source);
        staged.verify()?;
        let source = open(&directory.join("source.mkv"))?;
        let source_before = source.metadata()?;
        let (source_metadata, frames, seconds) = validate(&source, &directory, "source-decode", timeout, progress, 2, length).await?;
        ensure!((seconds - length).abs() <= 0.25, "source sample truncated or wrong duration");
        progress.stage(2, "Hashing validated shared sample", None, "");
        let mut hash_file = source.try_clone()?;
        report["source_sha256"] = json!(super::experimental::bounded_hash_progress(&mut hash_file,
            source.metadata()?.len(), timeout, None)?);
        report["source_metadata"] = source_metadata;
        report["source_decoded_frames"] = json!(frames);
        report["source_decoded_seconds"] = json!(seconds);
        for treatment in ["preserve", "yadif"] {
            let stage = if treatment == "preserve" { 3 } else { 5 };
            progress.stage(stage, &format!("Encoding {treatment}"), Some(seconds), "media seconds");
            verify_shared_source(&source, &source_before)?;
            let movie = create(&directory.join(format!("{treatment}.mp4")))?;
            let mut command = ffmpeg(limits.threads);
            command.args(["-protocol_whitelist", "file,pipe"]);
            input(&mut command, &source)?;
            command.args(["-map", "0:v:0", "-map", "0:a?", "-sn", "-map_metadata", "-1",
                "-map_chapters", "-1", "-c:v", "libx264", "-preset", "veryfast", "-crf", "20",
                "-pix_fmt", "yuv420p", "-threads", &limits.threads.to_string(), "-fps_mode", "passthrough",
                "-enc_time_base:v", "filter", "-c:a", "aac", "-ac", "2", "-b:a", "192k",
                "-movflags", "+faststart", "-progress", "pipe:1"]);
            if treatment == "yadif" { command.args(["-vf", wetube_core::YADIF_FILTER]); }
            output(&mut command, &movie, cap, "mp4")?;
            run(&mut command, &directory, &format!("encode-{treatment}"), timeout, false, progress).await?;
            progress.stage(stage, &format!("Syncing {treatment}"), None, "");
            movie.sync_all()?;
            drop(movie);
            let mut movie = open(&directory.join(format!("{treatment}.mp4")))?;
            let before = identity(&movie.metadata()?);
            let (metadata, output_frames, output_seconds) = validate(&movie, &directory,
                &format!("decode-{treatment}"), timeout, progress, stage + 1, seconds).await?;
            ensure!(frames == output_frames && (seconds - output_seconds).abs() <= 0.15,
                "comparison output lost frames or duration");
            let streams = metadata["streams"].as_array().context("missing output streams")?;
            ensure!(streams.iter().filter(|s| s["codec_type"] == "video").count() == 1
                && streams.len() == report["source_metadata"]["streams"].as_array().context("missing source streams")?.len(),
                "comparison stream layout changed");
            for stream in streams {
                ensure!(stream["nb_read_packets"].as_str().and_then(|n| n.parse::<u64>().ok()).is_some_and(|n| n > 0),
                    "empty encoded stream");
                match stream["codec_type"].as_str() {
                    Some("video") => ensure!(stream["codec_name"] == "h264" && stream["pix_fmt"] == "yuv420p",
                        "comparison is not browser-compatible H264"),
                    Some("audio") => ensure!(stream["codec_name"] == "aac" && stream["channels"] == 2, "comparison is not AAC stereo"),
                    _ => anyhow::bail!("unexpected comparison track"),
                }
            }
            let size = movie.metadata()?.len();
            progress.stage(stage + 1, &format!("Hashing validated {treatment}"), None, "");
            let digest = super::experimental::bounded_hash_progress(&mut movie, size, timeout, None)?;
            ensure!(identity(&movie.metadata()?) == before && size < cap, "clip changed or reached its byte limit");
            report[treatment] = json!({"metadata":metadata,"decoded_frames":output_frames,
                "decoded_seconds":output_seconds,"size_bytes":size,"sha256":digest,"file_identity":before,
                "filter":if treatment == "yadif" {Some(wetube_core::YADIF_FILTER)} else {None}});
        }
        progress.stage(7, "Finalizing comparison report", None, "");
        verify_shared_source(&source, &source_before)?;
        staged.verify()?;
        Ok(())
    }.await;
    match result {
        Ok(()) => report["status"] = json!("succeeded"),
        Err(error) => {
            report["status"] = json!("failed");
            report["error"] = json!(format!("{error:#}").chars().take(4096).collect::<String>());
        }
    }
    let mut file = create(&directory.join("comparison.json"))?;
    file.write_all(&serde_json::to_vec_pretty(&report)?)?;
    file.sync_all()?;
    File::open(&directory)?.sync_all()?;
    Ok(report)
}

pub async fn tick(
    pool: &SqlitePool,
    config: Arc<super::imports::ImportConfig>,
    limits: &super::conversions::Limits,
) -> Result<bool> {
    limits.validate()?;
    let Some(claim) = wetube_backend::queue::next(pool, "comparison").await? else {
        return Ok(false);
    };
    let key = claim.key;
    let result = run_job(pool, config, limits, claim.row).await;
    wetube_backend::queue::finish(pool, key).await?;
    result
}

pub(crate) async fn run_job(
    pool: &SqlitePool,
    config: Arc<super::imports::ImportConfig>,
    limits: &super::conversions::Limits,
    row: sqlx::sqlite::SqliteRow,
) -> Result<bool> {
    let job = comparisons::from_row(&row)?;
    let recording = Recording::start(
        pool,
        "comparison_jobs",
        &job.id,
        MediaWorkflow::Comparison,
        0,
    );
    let result: Result<Value> = async {
        let probe = sqlx::query(
            "SELECT * FROM probe_jobs WHERE id=? AND state='needs_review'
            AND report=? AND title=? AND angle=?",
        )
        .bind(&job.probe_job_id)
        .bind(row.try_get::<String, _>("probe_report")?)
        .bind(row.try_get::<i64, _>("title")?)
        .bind(row.try_get::<i64, _>("angle")?)
        .fetch_one(pool)
        .await?;
        generate(
            config,
            &probe_jobs::from_row(&probe)?,
            job.attempt_id.as_deref().context("no attempt")?,
            limits,
            &recording.progress,
        )
        .await
    }
    .await;
    let (state, report, error) = match result {
        Ok(report) => (
            report["status"].as_str().unwrap_or("failed").to_owned(),
            Some(report.to_string()),
            report["error"].as_str().map(str::to_owned),
        ),
        Err(error) => (
            "failed".into(),
            None,
            Some(format!("{error:#}").chars().take(4096).collect()),
        ),
    };
    sqlx::query("UPDATE comparison_jobs SET report=?,error=? WHERE id=? AND state='running'")
        .bind(report)
        .bind(error)
        .bind(&job.id)
        .execute(pool)
        .await?;
    wetube_backend::queue::defer_finish(pool, "comparison", &job.id, &state).await?;
    recording.finish().await;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_sample_guard_preserves_strict_identity_and_diagnoses_metadata_changes() -> Result<()>
    {
        let file = tempfile::NamedTempFile::new()?;
        std::fs::write(file.path(), b"sample")?;
        let source = open(file.path())?;
        let before = source.metadata()?;
        verify_shared_source(&source, &before)?;
        std::fs::write(file.path(), b"changed sample")?;
        let error = verify_shared_source(&source, &before)
            .unwrap_err()
            .to_string();
        assert!(error.contains("before [dev=") && error.contains("after [dev="));
        assert!(error.contains("size=6") && error.contains("size=14"));
        assert!(error.contains(" ino=") && error.contains(" mtime=") && error.contains(" ctime="));
        Ok(())
    }

    #[tokio::test]
    async fn comparison_revalidation_failure_retains_measured_progress_without_authorizing_clips()
    -> Result<()> {
        for reject_progress in [false, true] {
            let root = tempfile::tempdir()?;
            let incoming = root.path().join("incoming");
            let scratch = root.path().join("scratch");
            let state = root.path().join("state");
            std::fs::create_dir(&incoming)?;
            std::fs::create_dir(&state)?;
            let config = Arc::new(super::super::imports::ImportConfig::open(
                &incoming,
                &scratch,
                1024,
                1024,
                Duration::from_secs(5),
            )?);
            wetube_backend::db::initialize(&state).await?;
            let pool = wetube_backend::db::connect(&state).await?;
            sqlx::query("INSERT INTO users VALUES ('admin','admin','hash','admin')")
                .execute(&pool)
                .await?;
            let staged = uuid::Uuid::new_v4().to_string();
            std::fs::create_dir(scratch.join(&staged))?;
            std::fs::write(scratch.join(&staged).join("input.iso"), b"changed")?;
            let report = json!({"size_bytes":7,"sha256":"incorrect","staged_attempt_id":staged});
            sqlx::query("INSERT INTO probe_jobs(id,requested_by,name,source_identity,title,angle,state,created_at,attempt_id,report)
                VALUES ('probe','admin','test.iso','bound',1,1,'needs_review',1,?,?)")
                .bind(&staged).bind(report.to_string()).execute(&pool).await?;
            let queued = comparisons::enqueue(&pool, "admin", "probe")
                .await?
                .unwrap();
            assert!(queued.progress.is_none());
            if reject_progress {
                sqlx::query("CREATE TRIGGER reject_progress BEFORE UPDATE OF progress ON comparison_jobs BEGIN SELECT RAISE(FAIL,'injected telemetry failure'); END")
                    .execute(&pool).await?;
            }
            assert!(
                tick(
                    &pool,
                    config,
                    &super::super::conversions::Limits {
                        timeout: Duration::from_secs(5),
                        threads: 2,
                        max_output_bytes: CLIP_CAP,
                    }
                )
                .await?
            );
            let failed = comparisons::latest(&pool, "probe").await?.unwrap();
            assert_eq!(failed.state, wetube_core::JobState::Failed);
            assert!(
                failed
                    .error
                    .as_deref()
                    .unwrap()
                    .contains("checksum mismatch")
            );
            if reject_progress {
                assert!(failed.progress.is_none());
            } else {
                let progress = failed.progress.as_ref().unwrap();
                assert_eq!(progress.workflow, MediaWorkflow::Comparison);
                assert_eq!(progress.stage, 0);
                assert_eq!(progress.completed_units, Some(7.0));
                assert_eq!(progress.total_units, Some(7.0));
                assert_eq!(progress.rate_and_eta(), None);
                sqlx::query("UPDATE comparison_jobs SET state='running' WHERE id=?")
                    .bind(&queued.id)
                    .execute(&pool)
                    .await?;
                recover(&pool).await?;
                let interrupted = comparisons::latest(&pool, "probe").await?.unwrap();
                assert_eq!(interrupted.state, wetube_core::JobState::Failed);
                assert_eq!(interrupted.progress, failed.progress);
            }
            let attempt = scratch.join(failed.attempt_id.unwrap());
            assert!(attempt.join("comparison.json").is_file());
            assert!(!attempt.join("preserve.mp4").exists());
            assert_eq!(
                std::fs::read(scratch.join(&staged).join("input.iso"))?,
                b"changed"
            );
            assert_eq!(
                sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM movies")
                    .fetch_one(&pool)
                    .await?,
                0
            );
            pool.close().await;
        }
        Ok(())
    }

    #[tokio::test]
    async fn comparison_capture_and_validation_propagate_real_media_timestamps() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let pool = SqlitePool::connect("sqlite::memory:").await?;
        sqlx::query("CREATE TABLE comparison_jobs(id TEXT,state TEXT,progress TEXT)")
            .execute(&pool)
            .await?;
        sqlx::query("INSERT INTO comparison_jobs VALUES ('test','running',NULL)")
            .execute(&pool)
            .await?;
        let recording = Recording::start(
            &pool,
            "comparison_jobs",
            "test",
            MediaWorkflow::Comparison,
            1,
        );
        recording.progress.stage(
            1,
            "Extracting shared midpoint sample",
            Some(1.0),
            "media seconds",
        );
        let file = create(&directory.path().join("sample.mkv"))?;
        let mut command = ffmpeg(2);
        command.args([
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=160x90:rate=24",
            "-t",
            "1",
            "-c:v",
            "ffv1",
            "-progress",
            "pipe:1",
        ]);
        output(&mut command, &file, SOURCE_CAP, "matroska")?;
        run(
            &mut command,
            directory.path(),
            "extract",
            Duration::from_secs(10),
            false,
            &recording.progress,
        )
        .await?;
        recording.finish().await;
        let read = || async {
            let json: String = sqlx::query_scalar("SELECT progress FROM comparison_jobs")
                .fetch_one(&pool)
                .await?;
            anyhow::Ok(serde_json::from_str::<wetube_core::MediaProgress>(&json)?)
        };
        let extracted = read().await?;
        assert_eq!(extracted.stage, 1);
        assert!(extracted.completed_units.unwrap() > 0.8);
        assert!(extracted.fraction().unwrap() < 1.0);
        let recording = Recording::start(
            &pool,
            "comparison_jobs",
            "test",
            MediaWorkflow::Comparison,
            2,
        );
        let (_, frames, seconds) = validate(
            &file,
            directory.path(),
            "source-decode",
            Duration::from_secs(10),
            &recording.progress,
            2,
            1.0,
        )
        .await?;
        recording.finish().await;
        let decoded = read().await?;
        assert_eq!(decoded.stage, 2);
        assert_eq!(decoded.completed_units, Some(seconds));
        assert_eq!(frames, 24);
        assert_eq!(
            sqlx::query_scalar::<_, String>("SELECT state FROM comparison_jobs")
                .fetch_one(&pool)
                .await?,
            "running"
        );
        pool.close().await;
        Ok(())
    }

    #[test]
    fn midpoint_short_and_untrusted_seek_results() {
        assert_eq!(
            window(&json!({"format":{"duration":"120"}})).unwrap(),
            (55.0, 10.0)
        );
        assert_eq!(
            window(&json!({"format":{"duration":"3"}})).unwrap(),
            (0.0, 3.0)
        );
        assert!(window(&json!({"format":{"duration":"NaN"}})).is_err());
        assert!(seek_proof(b"could not seek to 3600; continuing").is_none());
        assert!(
            seek_proof(b"[dvdvideo] seeking: requested_nav_pts=4950000 new_nav_pts=4950000")
                .is_some()
        );
    }
}
