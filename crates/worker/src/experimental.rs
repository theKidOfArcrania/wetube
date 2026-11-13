//! Opt-in feasibility example, never used by the production job queue.
use anyhow::{Context, Result, ensure};
use clap::{Parser, ValueEnum};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Seek, Write},
    os::{
        fd::AsRawFd,
        unix::fs::{DirBuilderExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{process::Command, sync::watch};
use wetube_backend::{
    incoming::{IncomingRoot, identity},
    lock::MaintenanceLock,
};

#[derive(Clone, Copy, Debug, ValueEnum)]
enum VideoTreatment {
    Preserve,
    Yadif,
}

#[derive(Parser)]
#[command(about = "Experimental full DVD import; retains everything, never publishes or deletes")]
pub struct Options {
    #[arg(long)]
    iso: PathBuf,
    #[arg(long)]
    scratch_dir: PathBuf,
    #[arg(long, value_parser = clap::value_parser!(u8).range(1..=99))]
    title: u8,
    #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u8).range(1..=9))]
    angle: u8,
    #[arg(long, required = true)]
    copy_complete: bool,
    /// Explicit experimental choice, not a validated production encoding profile.
    #[arg(long, value_enum)]
    video_treatment: VideoTreatment,
    #[arg(long, default_value_t = 600, value_parser = clap::value_parser!(u64).range(1..=3600))]
    staging_timeout_secs: u64,
    /// Wall-clock limit for each media tool invocation.
    #[arg(long, default_value_t = 14400, value_parser = clap::value_parser!(u64).range(1..=86400))]
    tool_timeout_secs: u64,
    #[arg(long, default_value_t = 2, value_parser = clap::value_parser!(u8).range(1..=16))]
    threads: u8,
    /// Maximum bytes per output; capacity is checked for two such outputs.
    #[arg(long, default_value_t = 8 * 1024 * 1024 * 1024u64, value_parser = clap::value_parser!(u64).range(1048576..=68719476736))]
    max_output_bytes: u64,
    #[arg(long, default_value_t = 32 * 1024 * 1024 * 1024u64)]
    max_scratch_bytes: u64,
    /// Rehash and reuse input.iso from a prior experimental report; outputs always use a new attempt.
    #[arg(long)]
    resume_attempt: Option<uuid::Uuid>,
}

fn open_input(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    ensure!(
        file.metadata()?.is_file() && file.metadata()?.len() > 0,
        "input must be a non-empty regular file"
    );
    Ok(file)
}

fn output_file(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    // ffmpeg receives an already-created attempt-local file, not a writable user pathname.
    ensure!(
        unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFD, 0) } != -1,
        "inherit output descriptor"
    );
    Ok(file)
}

fn descriptor(file: &File) -> String {
    format!("/proc/self/fd/{}", file.as_raw_fd())
}

fn cancelled(cancel: &watch::Receiver<bool>) -> Result<()> {
    ensure!(
        !*cancel.borrow(),
        "experimental import interrupted; all attempt files retained"
    );
    Ok(())
}

async fn tool(
    phase: &str,
    command: &mut Command,
    timeout: Duration,
    cancel: &mut watch::Receiver<bool>,
    directory: &Path,
) -> Result<Vec<u8>> {
    cancelled(cancel)?;
    eprintln!("{phase}...");
    let started = Instant::now();
    let log_path = directory.join(format!("{phase}.stderr.txt"));
    let log = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(log_path)?;
    let stdout = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(directory.join(format!("{phase}.stdout.txt")))?;
    let outcome = tokio::select! {
        result = super::process::run_logged(command, timeout, 2 * 1024 * 1024, stdout.try_clone()?, log.try_clone()?) => result.with_context(|| phase.to_owned()),
        _ = cancel.changed() => Err(anyhow::anyhow!("{phase} interrupted; attempt retained")),
    };
    log.sync_all()?;
    stdout.sync_all()?;
    let output = outcome?;
    ensure!(
        output.status.success(),
        "{phase} failed: {}",
        String::from_utf8_lossy(&output.stderr[..output.stderr.len().min(4096)])
    );
    ensure!(
        output.stderr.is_empty(),
        "{phase} reported media errors despite a successful exit: {}",
        String::from_utf8_lossy(&output.stderr[..output.stderr.len().min(4096)])
    );
    eprintln!(
        "{phase} finished in {:.1}s",
        started.elapsed().as_secs_f64()
    );
    Ok(output.stdout)
}

fn dvd_input(command: &mut Command, options: &Options, file: &File) -> Result<()> {
    command
        .args([
            "-threads",
            &options.threads.to_string(),
            "-f",
            "dvdvideo",
            "-title",
            &options.title.to_string(),
            "-angle",
            &options.angle.to_string(),
            "-preindex",
            "0",
            "-i",
            "/proc/self/fd/0",
        ])
        .stdin(Stdio::from(file.try_clone()?));
    Ok(())
}

fn ffmpeg() -> Command {
    let mut command = Command::new("ffmpeg");
    command.args([
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
        "-stats_period",
        "10",
        "-progress",
        "pipe:1",
    ]);
    command
}

fn progress(bytes: &[u8]) -> Result<(u64, f64)> {
    let text = std::str::from_utf8(bytes)?;
    let mut frames = None;
    let mut time = None;
    let mut ended = false;
    for line in text.lines() {
        if let Some((key, value)) = line.split_once('=') {
            match key {
                "frame" => frames = value.trim().parse::<u64>().ok(),
                "out_time_us" => time = value.parse::<i64>().ok(),
                "progress" => ended = value == "end",
                _ => {}
            }
        }
    }
    let frames = frames.context("media tool did not report decoded frames")?;
    let seconds = time.context("media tool did not report a final timestamp")? as f64 / 1_000_000.0;
    ensure!(
        ended && frames > 0 && seconds.is_finite() && seconds > 0.0,
        "media tool did not finish a non-empty full decode"
    );
    Ok((frames, seconds))
}

fn duration(metadata: &Value) -> Result<f64> {
    let value: f64 = metadata["format"]["duration"]
        .as_str()
        .context("missing duration")?
        .parse()?;
    ensure!(value.is_finite() && value > 0.0, "invalid media duration");
    Ok(value)
}

fn same_duration(actual: f64, expected: f64) -> Result<()> {
    // DVD duration without preindexing is approximate; decoded frame equality is checked separately.
    ensure!(
        actual.is_finite() && (actual - expected).abs() <= 5.0 + expected * 0.001,
        "duration mismatch: {actual:.3}s versus {expected:.3}s"
    );
    Ok(())
}

fn streams<'a>(metadata: &'a Value, kind: &str) -> Vec<&'a Value> {
    metadata["streams"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|stream| stream["codec_type"] == kind)
        .collect()
}

fn same_language(source: &Value, output: &Value) -> Result<()> {
    if let Some(language) = source["tags"]["language"].as_str() {
        ensure!(
            output["tags"]["language"] == language,
            "track language was lost"
        );
    }
    Ok(())
}

fn validate_layout(source: &Value, movie: &Value, tracks: Option<&Value>) -> Result<()> {
    let source_video = streams(source, "video");
    let video = streams(movie, "video");
    ensure!(
        source_video.len() == 1 && video.len() == 1,
        "expected exactly one video stream"
    );
    ensure!(
        video[0]["codec_name"] == "h264" && video[0]["pix_fmt"] == "yuv420p",
        "unexpected encoded video format"
    );
    for key in ["width", "height"] {
        ensure!(
            video[0][key] == source_video[0][key],
            "encoded video dimensions changed"
        );
    }
    let audio = streams(source, "audio");
    let encoded = streams(movie, "audio");
    ensure!(
        encoded.len() == audio.len(),
        "encoded audio track count changed"
    );
    for (input, output) in audio.iter().zip(encoded) {
        ensure!(
            output["codec_name"] == "aac" && output["channels"] == 2,
            "expected AAC stereo fallback"
        );
        same_language(input, output)?;
    }
    let subtitles = streams(source, "subtitle");
    if !audio.is_empty() || !subtitles.is_empty() {
        let tracks = tracks.context("original tracks sidecar is missing")?;
        for kind in ["audio", "subtitle"] {
            let input = streams(source, kind);
            let output = streams(tracks, kind);
            ensure!(
                input.len() == output.len(),
                "original {kind} track count changed"
            );
            for (input, output) in input.iter().zip(output) {
                ensure!(
                    input["codec_name"] == output["codec_name"],
                    "original track codec changed"
                );
                if kind == "audio" {
                    ensure!(
                        input["channels"] == output["channels"],
                        "original channel count changed"
                    );
                    ensure!(
                        output["nb_read_packets"]
                            .as_str()
                            .and_then(|value| value.parse::<u64>().ok())
                            .is_some_and(|count| count > 0),
                        "original audio track has no packets"
                    );
                }
                same_language(input, output)?;
            }
        }
    }
    for stream in movie["streams"]
        .as_array()
        .context("missing output streams")?
    {
        if matches!(stream["codec_type"].as_str(), Some("video" | "audio")) {
            ensure!(
                stream["nb_read_packets"]
                    .as_str()
                    .and_then(|value| value.parse::<u64>().ok())
                    .is_some_and(|count| count > 0),
                "encoded track has no packets"
            );
        }
    }
    same_duration(duration(movie)?, duration(source)?)?;
    let chapters = |metadata: &Value| metadata["chapters"].as_array().map_or(0, Vec::len);
    ensure!(
        chapters(movie) == chapters(source),
        "movie chapter count changed"
    );
    if let Some(tracks) = tracks {
        ensure!(
            chapters(tracks) == chapters(source),
            "original track chapter count changed"
        );
    }
    Ok(())
}

async fn probe_output(
    path: &Path,
    format: &str,
    phase: &str,
    timeout: Duration,
    cancel: &mut watch::Receiver<bool>,
    directory: &Path,
) -> Result<Value> {
    let file = open_input(path)?;
    let bytes = tool(
        phase,
        Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-f",
                format,
                "-i",
                "/proc/self/fd/0",
                "-count_packets",
                "-show_format",
                "-show_streams",
                "-show_chapters",
                "-of",
                "json",
            ])
            .stdin(Stdio::from(file)),
        timeout,
        cancel,
        directory,
    )
    .await?;
    serde_json::from_slice(&bytes).context("invalid output probe JSON")
}

async fn media_pipeline(
    options: &Options,
    directory: &Path,
    source: &Value,
    file: &mut File,
    dvd: bool,
    cancel: &mut watch::Receiver<bool>,
) -> Result<Value> {
    let timeout = Duration::from_secs(options.tool_timeout_secs);
    file.rewind()?;
    let before = identity(&file.metadata()?);
    let mut decode = ffmpeg();
    if dvd {
        dvd_input(&mut decode, options, file)?;
    } else {
        decode
            .args([
                "-threads",
                &options.threads.to_string(),
                "-i",
                "/proc/self/fd/0",
            ])
            .stdin(Stdio::from(file.try_clone()?));
    }
    let source_progress = tool(
        "source-decode",
        decode.args([
            "-map",
            "0:v:0",
            "-map",
            "0:a?",
            "-sn",
            "-threads",
            &options.threads.to_string(),
            "-fps_mode",
            "passthrough",
            "-enc_time_base:v",
            "filter",
            "-f",
            "null",
            "-",
        ]),
        timeout,
        cancel,
        directory,
    )
    .await?;
    let (source_frames, source_seconds) = progress(&source_progress)?;
    same_duration(source_seconds, duration(source)?)?;
    ensure!(
        identity(&file.metadata()?) == before,
        "staged input changed during full decode"
    );

    let movie_path = directory.join("movie.mp4");
    let tracks_path = directory.join("original-tracks.mkv");
    let movie = output_file(&movie_path)?;
    let has_tracks =
        !streams(source, "audio").is_empty() || !streams(source, "subtitle").is_empty();
    let tracks = if has_tracks {
        Some(output_file(&tracks_path)?)
    } else {
        None
    };
    file.rewind()?;
    let mut encode = ffmpeg();
    if dvd {
        dvd_input(&mut encode, options, file)?;
    } else {
        encode
            .args([
                "-threads",
                &options.threads.to_string(),
                "-i",
                "/proc/self/fd/0",
            ])
            .stdin(Stdio::from(file.try_clone()?));
    }
    encode.args([
        "-y",
        "-map",
        "0:v:0",
        "-map",
        "0:a?",
        "-sn",
        "-map_metadata",
        "0",
        "-map_chapters",
        "0",
        "-c:v",
        "libx264",
        "-preset",
        "veryfast",
        "-crf",
        "20",
        "-pix_fmt",
        "yuv420p",
        "-threads",
        &options.threads.to_string(),
        "-filter_threads",
        "1",
        "-fps_mode",
        "passthrough",
        "-enc_time_base:v",
        "filter",
        "-c:a",
        "aac",
        "-ac",
        "2",
        "-b:a",
        "192k",
        "-fs",
        &options.max_output_bytes.to_string(),
        "-movflags",
        "+faststart",
    ]);
    if matches!(options.video_treatment, VideoTreatment::Yadif) {
        encode.args(["-vf", "yadif=mode=send_frame:parity=auto:deint=all"]);
    }
    encode.args(["-f", "mp4", &descriptor(&movie)]);
    if let Some(tracks) = &tracks {
        encode.args([
            "-map",
            "0:a?",
            "-map",
            "0:s?",
            "-map_metadata",
            "0",
            "-map_chapters",
            "0",
            "-c",
            "copy",
            "-fs",
            &options.max_output_bytes.to_string(),
            "-f",
            "matroska",
            &descriptor(tracks),
        ]);
    }
    tool("encode", &mut encode, timeout, cancel, directory).await?;
    ensure!(
        identity(&file.metadata()?) == before,
        "staged input changed during encoding"
    );
    movie.sync_all()?;
    if let Some(tracks) = &tracks {
        tracks.sync_all()?;
    }
    for output in std::iter::once(&movie).chain(tracks.iter()) {
        ensure!(
            output.metadata()?.len() > 0 && output.metadata()?.len() <= options.max_output_bytes,
            "encoded output is empty or exceeded its byte cap"
        );
    }
    drop(movie);
    drop(tracks);
    File::open(directory)?.sync_all()?;
    let movie_metadata = probe_output(
        &movie_path,
        "mov",
        "movie-probe",
        timeout,
        cancel,
        directory,
    )
    .await?;
    let tracks_metadata = if has_tracks {
        Some(
            probe_output(
                &tracks_path,
                "matroska",
                "tracks-probe",
                timeout,
                cancel,
                directory,
            )
            .await?,
        )
    } else {
        None
    };
    validate_layout(source, &movie_metadata, tracks_metadata.as_ref())?;

    let movie = open_input(&movie_path)?;
    let movie_identity = identity(&movie.metadata()?);
    let tracks = if has_tracks {
        Some(open_input(&tracks_path)?)
    } else {
        None
    };
    let mut verify = ffmpeg();
    verify
        .args([
            "-threads",
            &options.threads.to_string(),
            "-f",
            "mov",
            "-i",
            "/proc/self/fd/0",
        ])
        .stdin(Stdio::from(movie.try_clone()?));
    if let Some(tracks) = &tracks {
        ensure!(
            unsafe { libc::fcntl(tracks.as_raw_fd(), libc::F_SETFD, 0) } != -1,
            "inherit original tracks"
        );
        verify.args([
            "-threads",
            &options.threads.to_string(),
            "-err_detect",
            "explode",
            "-f",
            "matroska",
            "-i",
            &descriptor(tracks),
        ]);
    }
    verify.args(["-map", "0:v:0", "-map", "0:a?"]);
    if has_tracks {
        verify.args(["-map", "1:a?"]);
    }
    let verified = tool(
        "output-decode",
        verify.args([
            "-sn",
            "-threads",
            &options.threads.to_string(),
            "-fps_mode",
            "passthrough",
            "-enc_time_base:v",
            "filter",
            "-f",
            "null",
            "-",
        ]),
        timeout,
        cancel,
        directory,
    )
    .await?;
    let (output_frames, output_seconds) = progress(&verified)?;
    ensure!(
        identity(&movie.metadata()?) == movie_identity
            && identity(&std::fs::symlink_metadata(&movie_path)?) == movie_identity,
        "movie changed during verification"
    );
    ensure!(
        output_frames == source_frames,
        "decoded output frame count changed: {output_frames} versus {source_frames}"
    );
    same_duration(output_seconds, source_seconds)?;
    Ok(json!({
        "source_decoded_frames": source_frames, "source_decoded_seconds": source_seconds,
        "output_decoded_frames": output_frames, "output_decoded_seconds": output_seconds,
        "movie": movie_metadata, "original_tracks": tracks_metadata,
    }))
}

fn resume_staged(
    config: &super::imports::ImportConfig,
    previous: uuid::Uuid,
    attempt: &str,
    confirmed: &wetube_core::IncomingFile,
    title: u8,
    angle: u8,
    timeout: Duration,
) -> Result<super::imports::Staged> {
    let name = &confirmed.name;
    let size = confirmed.size_bytes;
    let root = config.scratch_path();
    let previous_path = root.join(previous.to_string());
    let mut bytes = Vec::new();
    open_input(&previous_path.join("report.json"))?
        .take(2 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= 2 * 1024 * 1024,
        "previous report exceeds its byte limit"
    );
    let report: Value = serde_json::from_slice(&bytes)?;
    ensure!(
        report["experimental"] == true && report["attempt_id"] == previous.to_string(),
        "resume requires a report from this experimental importer"
    );
    ensure!(
        report["source_name"].as_str() == Some(name.as_str())
            && report["title"] == title
            && report["angle"] == angle,
        "resume source name/title/angle does not match the previous attempt"
    );
    ensure!(
        report["input_size_bytes"].as_u64() == Some(size),
        "resume input size changed"
    );
    let expected = report["input_sha256"]
        .as_str()
        .context("previous attempt never completed verified staging")?;
    // Resumed attempts can refer back to the original staged attempt without copying the ISO.
    let staged_attempt = report["staged_attempt_id"]
        .as_str()
        .or_else(|| report["attempt_id"].as_str())
        .context("missing staged attempt ID")?;
    let staged_attempt = uuid::Uuid::parse_str(staged_attempt)?;
    let path = root.join(staged_attempt.to_string()).join("input.iso");
    let mut file = open_input(&path)?;
    let before = identity(&file.metadata()?);
    ensure!(file.metadata()?.len() == size, "retained ISO size changed");
    let deadline = Instant::now() + timeout;
    let mut digest = Sha256::new();
    let mut count = 0u64;
    let mut buffer = vec![0u8; 1024 * 1024];
    eprintln!("Rehashing retained input from attempt {staged_attempt}...");
    loop {
        ensure!(
            Instant::now() < deadline,
            "retained-input verification exceeded its staging budget"
        );
        let bytes = file.read(&mut buffer)?;
        if bytes == 0 {
            break;
        }
        count = count.checked_add(bytes as u64).context("size overflow")?;
        ensure!(count <= size, "retained ISO grew while verifying");
        digest.update(&buffer[..bytes]);
    }
    let sha256 = hex::encode(digest.finalize());
    ensure!(
        count == size && sha256 == expected,
        "retained ISO checksum mismatch; cannot resume"
    );
    ensure!(
        identity(&file.metadata()?) == before
            && identity(&std::fs::symlink_metadata(&path)?) == before,
        "retained ISO changed during verification"
    );
    let directory = root.join(attempt);
    std::fs::DirBuilder::new().mode(0o700).create(&directory)?;
    File::open(root)?.sync_all()?;
    file.rewind()?;
    Ok(super::imports::Staged {
        path,
        file,
        sha256,
        size_bytes: size,
        identity: before,
    })
}

async fn run_inner(options: Options, mut cancel: watch::Receiver<bool>) -> Result<PathBuf> {
    ensure!(
        options.copy_complete,
        "copy completion acknowledgement is required"
    );
    let incoming = options
        .iso
        .parent()
        .context("ISO needs a parent directory")?;
    let name = options
        .iso
        .file_name()
        .and_then(|name| name.to_str())
        .context("ISO needs a UTF-8 filename")?
        .to_owned();
    let root = IncomingRoot::open(incoming)?;
    let (source, original) = root.open_file(&name)?;
    let config = Arc::new(super::imports::ImportConfig::open(
        incoming,
        &options.scratch_dir,
        16 * 1024 * 1024 * 1024,
        options.max_scratch_bytes,
        Duration::from_secs(options.staging_timeout_secs),
    )?);
    let _lock = MaintenanceLock::acquire(&options.scratch_dir)?;
    let attempt = uuid::Uuid::new_v4().to_string();
    eprintln!("Experimental attempt {attempt}; inputs and partial output will be retained.");
    let stage_attempt = attempt.clone();
    let stage_config = config.clone();
    let expected = original.identity.clone();
    let stage_name = name.clone();
    let resume = options.resume_attempt;
    let title = options.title;
    let angle = options.angle;
    let confirmed = original.clone();
    let staging_timeout = Duration::from_secs(options.staging_timeout_secs);
    let started = Instant::now();
    let staged = tokio::task::spawn_blocking(move || {
        if let Some(previous) = resume {
            resume_staged(
                &stage_config,
                previous,
                &stage_attempt,
                &confirmed,
                title,
                angle,
                staging_timeout,
            )
        } else {
            super::imports::stage(&stage_config, &stage_name, &expected, &stage_attempt)
        }
    })
    .await?;
    let directory = options.scratch_dir.join(&attempt);
    let mut report = json!({
        "experimental": true, "published": false, "source_deletion_enabled": false,
        "attempt_id": attempt, "source_name": name, "title": options.title, "angle": options.angle,
        "profile": { "video": "H.264 CRF 20 veryfast yuv420p", "audio": "AAC stereo 192k per source audio track",
            "video_treatment": format!("{:?}", options.video_treatment), "original_tracks": "Matroska stream copy",
            "threads": options.threads, "max_bytes_per_output": options.max_output_bytes,
            "staging_timeout_secs": options.staging_timeout_secs, "tool_timeout_secs": options.tool_timeout_secs },
        "warnings": ["Experimental profile, not production publication or browser acceptance.",
            "Image subtitles are retained without OCR; chapter timing is approximate.",
            "Owner/mode checks do not prove NAS export privacy or SMB writer exclusion."]
    });
    let result: Result<()> = async {
        let mut staged = staged?;
        report["input_sha256"] = json!(staged.sha256);
        report["input_size_bytes"] = json!(staged.size_bytes);
        report["staged_identity"] = json!(staged.identity);
        report["source_identity_at_start"] = json!(original.identity);
        report["staged_attempt_id"] = json!(staged.path.parent().and_then(Path::file_name)
            .and_then(|name| name.to_str()).context("missing staged attempt ID")?);
        if let Some(previous) = options.resume_attempt { report["resumed_from"] = json!(previous.to_string()); }
        cancelled(&cancel)?;
        config.require_capacity(
            options
                .max_output_bytes
                .checked_mul(2)
                .context("output reservation overflow")?,
        )?;
        staged.verify()?;
        let inspection = tokio::select! {
            result = super::probe::inspect_open_iso(&staged.path, &staged.file, options.title, options.angle) => result?,
            _ = cancel.changed() => anyhow::bail!("DVD inspection interrupted; attempt retained"),
        };
        staged.verify()?;
        report["inspection"] = inspection;
        report["validation"] = media_pipeline(
            &options,
            &directory,
            &report["inspection"]["metadata"],
            &mut staged.file,
            true,
            &mut cancel,
        )
        .await?;
        staged.verify()?;
        root.verify(&name, &source, &original.identity)?;
        cancelled(&cancel)?;
        Ok(())
    }
    .await;
    report["elapsed_seconds"] = json!(started.elapsed().as_secs_f64());
    report["status"] = json!(if result.is_ok() {
        "verified_experimental_output"
    } else {
        "failed"
    });
    if let Err(error) = &result {
        report["error"] = json!(format!("{error:#}"));
    }
    let report_path = directory.join("report.json");
    if directory.is_dir() {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&report_path)?;
        file.write_all(&serde_json::to_vec_pretty(&report)?)?;
        file.sync_all()?;
        File::open(&directory)?.sync_all()?;
        eprintln!("Retained report: {}", report_path.display());
    }
    result?;
    Ok(report_path)
}

/// Run without production state. On shutdown drain blocking staging, but cancel active media children.
pub async fn run(options: Options) -> Result<PathBuf> {
    let (send, receive) = watch::channel(false);
    let work = run_inner(options, receive);
    tokio::pin!(work);
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        result = &mut work => result,
        _ = async { tokio::select! {
            _ = tokio::signal::ctrl_c() => {},
            _ = terminate.recv() => {},
        }} => {
            let _ = send.send(true);
            eprintln!("Stopping; waiting for any blocking staging I/O to finish.");
            work.await
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cancelled_staging_keeps_its_verified_resume_fields() -> Result<()> {
        let incoming = tempfile::tempdir()?;
        let parent = tempfile::tempdir()?;
        let scratch = parent.path().join("scratch");
        let iso = incoming.path().join("completed.iso");
        std::fs::write(&iso, b"completed test copy")?;
        let options = Options::try_parse_from([
            "import_dvd",
            "--iso",
            iso.to_str().unwrap(),
            "--scratch-dir",
            scratch.to_str().unwrap(),
            "--title",
            "1",
            "--copy-complete",
            "--video-treatment",
            "preserve",
        ])?;
        let (_send, cancel) = watch::channel(true);
        let error = run_inner(options, cancel)
            .await
            .err()
            .context("cancelled import succeeded")?;
        assert!(error.to_string().contains("interrupted"));
        let attempt = std::fs::read_dir(&scratch)?
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .find(|path| path.is_dir())
            .context("no retained cancelled attempt")?;
        let report: Value = serde_json::from_slice(&std::fs::read(attempt.join("report.json"))?)?;
        assert_eq!(
            report["input_sha256"],
            hex::encode(Sha256::digest(b"completed test copy"))
        );
        assert_eq!(report["input_size_bytes"], 19);
        let previous = uuid::Uuid::parse_str(report["attempt_id"].as_str().unwrap())?;
        let config = super::super::imports::ImportConfig::open(
            incoming.path(),
            &scratch,
            1024,
            4096,
            Duration::from_secs(30),
        )?;
        let confirmed = IncomingRoot::open(incoming.path())?
            .open_file("completed.iso")?
            .1;
        let resumed = resume_staged(
            &config,
            previous,
            &uuid::Uuid::new_v4().to_string(),
            &confirmed,
            1,
            1,
            Duration::from_secs(30),
        )?;
        assert_eq!(resumed.sha256, report["input_sha256"]);
        Ok(())
    }

    #[tokio::test]
    async fn invalid_dvd_retains_staged_input_and_a_failed_report() -> Result<()> {
        let incoming = tempfile::tempdir()?;
        let parent = tempfile::tempdir()?;
        let scratch = parent.path().join("scratch");
        let iso = incoming.path().join("invalid.iso");
        std::fs::write(&iso, b"not a DVD")?;
        let options = Options::try_parse_from([
            "import_dvd",
            "--iso",
            iso.to_str().unwrap(),
            "--scratch-dir",
            scratch.to_str().unwrap(),
            "--title",
            "1",
            "--copy-complete",
            "--video-treatment",
            "preserve",
            "--tool-timeout-secs",
            "30",
            "--max-output-bytes",
            "1048576",
        ])?;
        let (_send, cancel) = watch::channel(false);
        let error = run_inner(options, cancel.clone())
            .await
            .err()
            .context("invalid DVD unexpectedly succeeded")?;
        let directory = std::fs::read_dir(&scratch)?
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .find(|path| path.is_dir())
            .with_context(|| format!("no retained attempt after: {error:#}"))?;
        let report: Value = serde_json::from_slice(&std::fs::read(directory.join("report.json"))?)?;
        assert_eq!(report["status"], "failed");
        assert_eq!(report["published"], false);
        assert!(report["error"].as_str().unwrap().contains("DVD inspection"));
        assert_eq!(std::fs::read(directory.join("input.iso"))?, b"not a DVD");
        assert_eq!(std::fs::read(&iso)?, b"not a DVD");
        assert!(!directory.join("movie.mp4").exists());
        let previous = uuid::Uuid::parse_str(report["attempt_id"].as_str().unwrap())?;
        let config = super::super::imports::ImportConfig::open(
            incoming.path(),
            &scratch,
            1024,
            4096,
            Duration::from_secs(30),
        )?;
        let confirmed = IncomingRoot::open(incoming.path())?
            .open_file("invalid.iso")?
            .1;
        let resumed = uuid::Uuid::new_v4().to_string();
        let staged = resume_staged(
            &config,
            previous,
            &resumed,
            &confirmed,
            1,
            1,
            Duration::from_secs(30),
        )?;
        assert_eq!(staged.sha256, report["input_sha256"]);
        assert!(!scratch.join(&resumed).join("input.iso").exists());
        assert!(
            resume_staged(
                &config,
                previous,
                &uuid::Uuid::new_v4().to_string(),
                &confirmed,
                2,
                1,
                Duration::from_secs(30)
            )
            .is_err()
        );
        std::fs::write(directory.join("input.iso"), b"bad a DVD")?;
        let error = resume_staged(
            &config,
            previous,
            &uuid::Uuid::new_v4().to_string(),
            &confirmed,
            1,
            1,
            Duration::from_secs(30),
        )
        .err()
        .context("corrupt resume unexpectedly succeeded")?;
        assert!(error.to_string().contains("checksum mismatch"), "{error:#}");
        Ok(())
    }

    #[test]
    fn refuses_incomplete_progress_and_lost_tracks() {
        assert!(progress(b"frame=50\nout_time_us=2000000\nprogress=continue\n").is_err());
        assert!(progress(b"frame=0\nout_time_us=0\nprogress=end\n").is_err());
        assert!(same_duration(20.0, 100.0).is_err());
        let source = json!({"format":{"duration":"10"}, "streams":[
            {"codec_type":"video","width":720,"height":480},
            {"codec_type":"audio","codec_name":"ac3","channels":6,"tags":{"language":"eng"}}]});
        let movie = json!({"format":{"duration":"10"}, "streams":[
            {"codec_type":"video","codec_name":"h264","pix_fmt":"yuv420p","width":720,"height":480,"nb_read_packets":"300"},
            {"codec_type":"audio","codec_name":"aac","channels":2,"tags":{"language":"eng"},"nb_read_packets":"400"}]});
        let mut tracks = json!({"streams":[source["streams"][1].clone()]});
        tracks["streams"][0]["nb_read_packets"] = json!("400");
        validate_layout(&source, &movie, Some(&tracks)).unwrap();
        assert!(validate_layout(&source, &movie, None).is_err());
        let mut lost = movie.clone();
        lost["streams"][1]["tags"]["language"] = json!("fra");
        assert!(validate_layout(&source, &lost, Some(&tracks)).is_err());
        lost = movie;
        lost["streams"][1]["nb_read_packets"] = json!("0");
        assert!(validate_layout(&source, &lost, Some(&tracks)).is_err());
    }

    #[tokio::test]
    async fn synthetic_media_runs_full_decode_encode_and_validation() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("fixture.mkv");
        let chapters = dir.path().join("chapters.ffmetadata");
        let captions = dir.path().join("captions.srt");
        std::fs::write(
            &chapters,
            ";FFMETADATA1\n[CHAPTER]\nTIMEBASE=1/1000\nSTART=0\nEND=800\ntitle=Opening\n[CHAPTER]\nTIMEBASE=1/1000\nSTART=800\nEND=1600\ntitle=Ending\n",
        )?;
        std::fs::write(
            &captions,
            "1\n00:00:00,200 --> 00:00:01,200\nGenerated test subtitle\n",
        )?;
        let output = super::super::process::run(
            Command::new("ffmpeg")
                .args([
                    "-hide_banner",
                    "-nostdin",
                    "-v",
                    "error",
                    "-f",
                    "lavfi",
                    "-i",
                    "testsrc2=size=128x96:rate=25",
                    "-f",
                    "lavfi",
                    "-i",
                    "sine=frequency=440:sample_rate=48000",
                    "-f",
                    "ffmetadata",
                    "-i",
                    chapters.to_str().unwrap(),
                    "-f",
                    "srt",
                    "-i",
                    captions.to_str().unwrap(),
                    "-map",
                    "0:v:0",
                    "-map",
                    "1:a:0",
                    "-map",
                    "3:s:0",
                    "-map_metadata",
                    "2",
                    "-map_chapters",
                    "2",
                    "-t",
                    "2",
                    "-c:v",
                    "ffv1",
                    "-vf",
                    "settb=1/1000,setpts=0.8*PTS",
                    "-enc_time_base:v",
                    "1/1000",
                    "-fps_mode",
                    "passthrough",
                    "-frames:v",
                    "50",
                    "-threads",
                    "1",
                    "-c:a",
                    "ac3",
                    "-ac",
                    "6",
                    "-metadata:s:a:0",
                    "language=eng",
                    "-c:s",
                    "copy",
                    "-metadata:s:s:0",
                    "language=eng",
                    "-f",
                    "matroska",
                ])
                .arg(&path),
            Duration::from_secs(30),
            65536,
        )
        .await?;
        ensure!(
            output.status.success(),
            "fixture: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let (_send, mut cancel) = watch::channel(false);
        let metadata = probe_output(
            &path,
            "matroska",
            "fixture-probe",
            Duration::from_secs(30),
            &mut cancel,
            dir.path(),
        )
        .await?;
        let mut options = Options::try_parse_from([
            "import_dvd",
            "--iso",
            "fixture.iso",
            "--scratch-dir",
            "unused",
            "--title",
            "1",
            "--copy-complete",
            "--video-treatment",
            "yadif",
            "--tool-timeout-secs",
            "30",
        ])?;
        let mut input = open_input(&path)?;
        let validation = media_pipeline(
            &options,
            dir.path(),
            &metadata,
            &mut input,
            false,
            &mut cancel,
        )
        .await?;
        assert_eq!(validation["source_decoded_frames"], 50);
        assert_eq!(validation["output_decoded_frames"], 50);
        assert_eq!(validation["movie"]["chapters"].as_array().unwrap().len(), 2);
        assert_eq!(
            validation["original_tracks"]["chapters"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(streams(&validation["original_tracks"], "subtitle").len(), 1);
        let mut lost_chapters = validation["movie"].clone();
        lost_chapters["chapters"] = json!([]);
        assert!(
            validate_layout(
                &metadata,
                &lost_chapters,
                Some(&validation["original_tracks"])
            )
            .is_err()
        );
        assert!(dir.path().join("movie.mp4").metadata()?.len() > 0);
        assert!(dir.path().join("original-tracks.mkv").metadata()?.len() > 0);
        let failed = tempfile::tempdir()?;
        options.max_output_bytes = 1024;
        assert!(
            media_pipeline(
                &options,
                failed.path(),
                &metadata,
                &mut input,
                false,
                &mut cancel,
            )
            .await
            .is_err()
        );
        assert!(failed.path().join("movie.mp4").exists());
        Ok(())
    }
}
