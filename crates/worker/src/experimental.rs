//! Opt-in feasibility example, never used by the production job queue.
use crate::progress::Progress;
use anyhow::{Context, Result, ensure};
use clap::{Parser, ValueEnum};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Seek, Write},
    os::{
        fd::AsRawFd,
        unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
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

type LiveJob<'a> = Option<(&'a sqlx::SqlitePool, &'a str, &'a Progress)>;

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
    tool_progress(phase, command, timeout, cancel, directory, None).await
}

async fn tool_progress(
    phase: &str,
    command: &mut Command,
    timeout: Duration,
    cancel: &mut watch::Receiver<bool>,
    directory: &Path,
    progress: Option<&Progress>,
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
        result = async {
            if matches!(phase, "source-decode" | "encode" | "output-decode") {
                super::process::run_logged_progress(command, timeout, 2 * 1024 * 1024, stdout.try_clone()?, log.try_clone()?, progress).await
            } else {
                super::process::run_logged(command, timeout, 2 * 1024 * 1024, stdout.try_clone()?, log.try_clone()?).await
            }
        } => result.with_context(|| phase.to_owned()),
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
    command.env("DVDCSS_CACHE", "off");
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
        "1",
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

fn metadata_snapshot(metadata: &std::fs::Metadata) -> String {
    format!(
        "dev={} inode={} bytes={} mtime={}.{:09} ctime={}.{:09}",
        metadata.dev(),
        metadata.ino(),
        metadata.len(),
        metadata.mtime(),
        metadata.mtime_nsec(),
        metadata.ctime(),
        metadata.ctime_nsec(),
    )
}

async fn media_pipeline(
    options: &Options,
    directory: &Path,
    source: &Value,
    file: &mut File,
    dvd: bool,
    cancel: &mut watch::Receiver<bool>,
    job: LiveJob<'_>,
) -> Result<Value> {
    let timeout = Duration::from_secs(options.tool_timeout_secs);
    file.rewind()?;
    let before_metadata = file.metadata()?;
    let before = identity(&before_metadata);
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
    phase(job, "source_decode").await?;
    if let Some((_, _, progress)) = job {
        progress.stage(
            1,
            "Full source decode",
            Some(duration(source)?),
            "media seconds",
        );
    }
    let source_progress = tool_progress(
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
        job.map(|(_, _, progress)| progress),
    )
    .await?;
    let (source_frames, source_seconds) = progress(&source_progress)?;
    same_duration(source_seconds, duration(source)?)?;
    let after_decode = file.metadata()?;
    ensure!(
        identity(&after_decode) == before,
        "staged input changed during full decode (before: {}; after: {})",
        metadata_snapshot(&before_metadata),
        metadata_snapshot(&after_decode),
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
        encode.args(["-vf", wetube_core::YADIF_FILTER]);
    }
    // FFmpeg emits dangling QuickTime references when every chapter lacks a title.
    if let Some(chapters) = source["chapters"].as_array() {
        for (index, chapter) in chapters.iter().enumerate() {
            if chapter["tags"]["title"]
                .as_str()
                .is_none_or(|title| title.trim().is_empty())
            {
                encode.args([
                    format!("-metadata:c:{index}"),
                    format!("title=Chapter {}", index + 1),
                ]);
            }
        }
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
    phase(job, "encoding").await?;
    if let Some((_, _, progress)) = job {
        progress.stage(
            2,
            "Encoding movie and preserving original tracks",
            Some(source_seconds),
            "media seconds",
        );
    }
    tool_progress(
        "encode",
        &mut encode,
        timeout,
        cancel,
        directory,
        job.map(|(_, _, progress)| progress),
    )
    .await?;
    let after_encode = file.metadata()?;
    ensure!(
        identity(&after_encode) == before,
        "staged input changed during encoding (before: {}; after: {})",
        metadata_snapshot(&before_metadata),
        metadata_snapshot(&after_encode),
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
    let movie_snapshot = identity(&open_input(&movie_path)?.metadata()?);
    let tracks_snapshot = if has_tracks {
        Some(identity(&open_input(&tracks_path)?.metadata()?))
    } else {
        None
    };
    phase(job, "validating").await?;
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
    ensure!(
        movie_identity == movie_snapshot,
        "movie changed during probe validation"
    );
    let tracks = if has_tracks {
        Some(open_input(&tracks_path)?)
    } else {
        None
    };
    if let Some(tracks) = &tracks {
        ensure!(
            Some(identity(&tracks.metadata()?)) == tracks_snapshot,
            "original tracks changed during probe validation"
        );
    }
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
    if let Some((_, _, progress)) = job {
        progress.stage(
            3,
            "Full output decode",
            Some(source_seconds),
            "media seconds",
        );
    }
    let verified = tool_progress(
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
        job.map(|(_, _, progress)| progress),
    )
    .await?;
    let (output_frames, output_seconds) = progress(&verified)?;
    ensure!(
        identity(&movie.metadata()?) == movie_identity
            && identity(&std::fs::symlink_metadata(&movie_path)?) == movie_identity,
        "movie changed during verification"
    );
    if let Some(tracks) = &tracks {
        ensure!(
            Some(identity(&tracks.metadata()?)) == tracks_snapshot
                && Some(identity(&std::fs::symlink_metadata(&tracks_path)?)) == tracks_snapshot,
            "original tracks changed during verification"
        );
    }
    ensure!(
        output_frames == source_frames,
        "decoded output frame count changed: {output_frames} versus {source_frames}"
    );
    same_duration(output_seconds, source_seconds)?;
    Ok(json!({
        "source_decoded_frames": source_frames, "source_decoded_seconds": source_seconds,
        "output_decoded_frames": output_frames, "output_decoded_seconds": output_seconds,
        "movie": movie_metadata, "original_tracks": tracks_metadata,
        "movie_identity":movie_snapshot,"original_tracks_identity":tracks_snapshot,
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
        _directory: None,
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
            "Filesystem access controls and writer exclusion must be verified separately."]
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
                .and_then(|bytes|bytes.checked_add(32*1024*1024))
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
            None,
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

async fn phase(job: LiveJob<'_>, phase: &str) -> Result<()> {
    if let Some((pool, id, progress)) = job {
        let stage = match phase {
            "source_decode" => 1,
            "encoding" => 2,
            "validating" => 3,
            _ => 0,
        };
        progress.stage(stage, phase, None, "");
        sqlx::query("UPDATE conversion_jobs SET phase=? WHERE id=? AND state='converting'")
            .bind(phase)
            .bind(id)
            .execute(pool)
            .await?;
    }
    Ok(())
}

pub(crate) fn bounded_hash_progress(
    file: &mut File,
    size: u64,
    timeout: Duration,
    progress: Option<&Progress>,
) -> Result<String> {
    file.rewind()?;
    let before = identity(&file.metadata()?);
    let deadline = Instant::now() + timeout;
    let mut digest = Sha256::new();
    let mut count = 0u64;
    let mut buffer = [0u8; 128 * 1024];
    loop {
        ensure!(
            Instant::now() < deadline,
            "checksum verification deadline exceeded"
        );
        let bytes = file.read(&mut buffer)?;
        if bytes == 0 {
            break;
        }
        count = count.checked_add(bytes as u64).context("hash overflow")?;
        ensure!(count <= size, "input grew during checksum verification");
        digest.update(&buffer[..bytes]);
        if let Some(progress) = progress {
            progress.units(count as f64);
        }
    }
    ensure!(
        count == size && identity(&file.metadata()?) == before,
        "input changed during checksum verification"
    );
    file.rewind()?;
    Ok(hex::encode(digest.finalize()))
}

pub(crate) async fn convert(
    pool: &sqlx::SqlitePool,
    config: Arc<super::imports::ImportConfig>,
    job: &wetube_core::ConversionJob,
    probe: &wetube_core::ProbeJob,
    limits: &super::conversions::Limits,
    progress: &Progress,
) -> Result<Value> {
    let attempt = job
        .attempt_id
        .as_deref()
        .context("missing conversion attempt")?;
    let directory = config.scratch_path().join(attempt);
    std::fs::DirBuilder::new().mode(0o700).create(&directory)?;
    File::open(config.scratch_path())?.sync_all()?;
    let mut report = json!({
        "experimental":true,"published":false,"source_deletion_enabled":false,
        "attempt_id":attempt,"probe_job_id":probe.id,"source_name":probe.name,
        "title":probe.title,"angle":probe.angle,"profile":{"video_treatment":job.video_treatment},
        "warnings":["Local-library profile only. Original inputs and tracks retained; no OCR or source cleanup."]
    });
    let result: Result<()> = async {
        let staged_config = config.clone();
        let staged_probe = probe.clone();
        phase(Some((pool, &job.id, progress)), "revalidating_input").await?;
        let staging_progress = progress.clone();
        let mut staged = tokio::task::spawn_blocking(move || {
            super::imports::revalidate_progress(
                &staged_config,
                &staged_probe,
                Some(&staging_progress),
            )
        })
        .await??;
        report["input_sha256"] = json!(staged.sha256);
        report["input_size_bytes"] = json!(staged.size_bytes);
        report["staged_identity"] = json!(staged.identity);
        report["staged_attempt_id"] = probe
            .report
            .as_ref()
            .and_then(|report| report.get("staged_attempt_id"))
            .cloned()
            .unwrap_or_else(|| json!(probe.attempt_id));
        config.require_capacity(
            limits
                .max_output_bytes
                .checked_mul(2)
                .and_then(|bytes| bytes.checked_add(32 * 1024 * 1024))
                .context("output overflow")?,
        )?;
        staged.verify()?;
        progress.stage(0, "Reinspecting checksum-bound program", None, "");
        let inspection =
            super::probe::inspect_open_iso(&staged.path, &staged.file, probe.title, probe.angle)
                .await?;
        staged.verify()?;
        report["inspection"] = inspection;
        let options = Options {
            iso: staged.path.clone(),
            scratch_dir: config.scratch_path(),
            title: probe.title,
            angle: probe.angle,
            copy_complete: true,
            video_treatment: match job.video_treatment {
                wetube_core::VideoTreatment::Preserve => VideoTreatment::Preserve,
                wetube_core::VideoTreatment::Yadif => VideoTreatment::Yadif,
            },
            staging_timeout_secs: 600,
            tool_timeout_secs: limits.timeout.as_secs(),
            threads: limits.threads,
            max_output_bytes: limits.max_output_bytes,
            max_scratch_bytes: 0,
            resume_attempt: None,
        };
        let (_sender, mut cancel) = watch::channel(false);
        report["validation"] = media_pipeline(
            &options,
            &directory,
            &report["inspection"]["metadata"],
            &mut staged.file,
            true,
            &mut cancel,
            Some((pool, &job.id, progress)),
        )
        .await?;
        staged.verify()?;
        for name in ["movie.mp4", "original-tracks.mkv"] {
            let path = directory.join(name);
            if name == "original-tracks.mkv" && report["validation"]["original_tracks"].is_null() {
                continue;
            }
            let mut file = open_input(&path)?;
            let before = identity(&file.metadata()?);
            let expected_identity = report["validation"][if name == "movie.mp4" {
                "movie_identity"
            } else {
                "original_tracks_identity"
            }]
            .as_str()
            .context("validation missing output identity")?;
            ensure!(
                before == expected_identity,
                "validated output changed before final hashing"
            );
            let size = file.metadata()?.len();
            let timeout = limits.timeout;
            progress.stage(
                3,
                &format!("Checksum validation: {name}"),
                Some(size as f64),
                "bytes",
            );
            let hashing_progress = progress.clone();
            let digest = tokio::task::spawn_blocking(move || {
                bounded_hash_progress(&mut file, size, timeout, Some(&hashing_progress))
            })
            .await??;
            ensure!(
                identity(&open_input(&path)?.metadata()?) == before,
                "output replaced while hashing"
            );
            report[if name == "movie.mp4" {
                "movie_sha256"
            } else {
                "original_tracks_sha256"
            }] = json!(digest);
        }
        staged.verify()?;
        Ok(())
    }
    .await;
    report["status"] = json!(if result.is_ok() {
        "verified_experimental_output"
    } else {
        "failed"
    });
    if let Err(error) = &result {
        report["error"] = json!(format!("{error:#}"));
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(directory.join("report.json"))?;
    file.write_all(&serde_json::to_vec_pretty(&report)?)?;
    file.sync_all()?;
    File::open(&directory)?.sync_all()?;
    // The caller persists even failed reports, never treating old output as a successful retry.
    Ok(report)
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
        for reject_progress in [false, true] {
            synthetic_media_case(reject_progress, false).await?;
        }
        Ok(())
    }

    #[tokio::test]
    async fn untitled_chapters_survive_conversion_and_publication() -> Result<()> {
        synthetic_media_case(false, true).await
    }

    async fn synthetic_media_case(reject_progress: bool, untitled_chapters: bool) -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("fixture.mkv");
        let chapters = dir.path().join("chapters.ffmetadata");
        let captions = dir.path().join("captions.srt");
        std::fs::write(
            &chapters,
            if untitled_chapters {
                ";FFMETADATA1\n[CHAPTER]\nTIMEBASE=1/1000\nSTART=0\nEND=800\n[CHAPTER]\nTIMEBASE=1/1000\nSTART=800\nEND=1600\n"
            } else {
                ";FFMETADATA1\n[CHAPTER]\nTIMEBASE=1/1000\nSTART=0\nEND=800\ntitle=Opening\n[CHAPTER]\nTIMEBASE=1/1000\nSTART=800\nEND=1600\ntitle=Ending\n"
            },
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
        let telemetry = sqlx::SqlitePool::connect("sqlite::memory:").await?;
        sqlx::query("CREATE TABLE conversion_jobs(id TEXT,state TEXT,phase TEXT,progress TEXT)")
            .execute(&telemetry)
            .await?;
        sqlx::query("INSERT INTO conversion_jobs(id,state) VALUES ('generated','converting')")
            .execute(&telemetry)
            .await?;
        let recording = crate::progress::Recording::start(
            &telemetry,
            "conversion_jobs",
            "generated",
            wetube_core::MediaWorkflow::Conversion,
            0,
        );
        let validation = media_pipeline(
            &options,
            dir.path(),
            &metadata,
            &mut input,
            false,
            &mut cancel,
            Some((&telemetry, "generated", &recording.progress)),
        )
        .await?;
        recording.finish().await;
        let snapshot: String = sqlx::query_scalar("SELECT progress FROM conversion_jobs")
            .fetch_one(&telemetry)
            .await?;
        let snapshot: wetube_core::MediaProgress = serde_json::from_str(&snapshot)?;
        assert_eq!(snapshot.stage, 3);
        assert_eq!(snapshot.phase, "Full output decode");
        assert!(snapshot.completed_units.unwrap() > 0.0);
        assert!(snapshot.fraction().unwrap() <= 0.99);
        assert_eq!(
            sqlx::query_scalar::<_, String>("SELECT state FROM conversion_jobs")
                .fetch_one(&telemetry)
                .await?,
            "converting"
        );
        telemetry.close().await;
        assert_eq!(validation["source_decoded_frames"], 50);
        assert_eq!(validation["output_decoded_frames"], 50);
        assert_eq!(validation["movie"]["chapters"].as_array().unwrap().len(), 2);
        let expected_titles = if untitled_chapters {
            ["Chapter 1", "Chapter 2"]
        } else {
            ["Opening", "Ending"]
        };
        for (chapter, title) in validation["movie"]["chapters"]
            .as_array()
            .unwrap()
            .iter()
            .zip(expected_titles)
        {
            assert_eq!(chapter["tags"]["title"], title);
        }
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
        let library = tempfile::tempdir()?;
        let state = library.path().join("state");
        let media = library.path().join("media");
        let incoming = library.path().join("incoming");
        std::fs::create_dir(&state)?;
        std::fs::create_dir(&media)?;
        std::fs::create_dir(&incoming)?;
        wetube_backend::db::initialize(&state).await?;
        let pool = wetube_backend::db::connect(&state).await?;
        sqlx::query("INSERT INTO users VALUES ('admin','admin','hash','admin')")
            .execute(&pool)
            .await?;
        sqlx::query("INSERT INTO probe_jobs(id,requested_by,name,source_identity,title,angle,state,created_at,attempt_id,report) VALUES('probe','admin','fixture.iso','identity',1,1,'needs_review',1,'staged','{}')")
            .execute(&pool).await?;
        let attempt = uuid::Uuid::new_v4().to_string();
        let output_directory = dir.path().join(&attempt);
        std::fs::create_dir(&output_directory)?;
        // Move only generated outputs to the UUID attempt layout used by admission.
        for name in ["movie.mp4", "original-tracks.mkv"] {
            std::fs::rename(dir.path().join(name), output_directory.join(name))?;
        }
        let movie_bytes = std::fs::read(output_directory.join("movie.mp4"))?;
        let tracks_bytes = std::fs::read(output_directory.join("original-tracks.mkv"))?;
        let movie_digest = hex::encode(Sha256::digest(&movie_bytes));
        let report = json!({
            "experimental":true,"published":false,"source_deletion_enabled":false,
            "status":"verified_experimental_output","attempt_id":attempt,"validation":validation,
            "movie_sha256":movie_digest,"original_tracks_sha256":hex::encode(Sha256::digest(&tracks_bytes))
        });
        std::fs::write(
            output_directory.join("report.json"),
            serde_json::to_vec_pretty(&report)?,
        )?;
        let job = wetube_backend::conversions::enqueue(
            &pool,
            "admin",
            &wetube_core::ConversionRequest {
                probe_job_id: "probe".into(),
                video_treatment: wetube_core::VideoTreatment::Yadif,
            },
        )
        .await?
        .context("conversion not queued")?;
        sqlx::query("UPDATE conversion_jobs SET state='needs_review',attempt_id=?,report=?,movie_sha256=? WHERE id=?")
            .bind(&attempt).bind(report.to_string()).bind(&movie_digest).bind(&job.id).execute(&pool).await?;
        wetube_backend::conversions::publish(
            &pool,
            &job.id,
            &wetube_core::PublishRequest {
                title: "Verified fixture".into(),
                year: Some(2026),
                description: "Generated media".into(),
                movie_sha256: movie_digest,
                confirm: true,
            },
        )
        .await?
        .context("publication not queued")?;
        let config = Arc::new(super::super::imports::ImportConfig::open(
            &incoming,
            dir.path(),
            1024 * 1024,
            64 * 1024 * 1024,
            Duration::from_secs(30),
        )?);
        let media = wetube_backend::preview::PreviewRoot::open(&media)?;
        let limits = super::super::conversions::Limits {
            timeout: Duration::from_secs(30),
            threads: 1,
            max_output_bytes: 8 * 1024 * 1024,
        };
        if reject_progress {
            sqlx::query("CREATE TRIGGER reject_conversion_progress BEFORE UPDATE OF progress ON conversion_jobs BEGIN SELECT RAISE(FAIL,'injected telemetry failure'); END").execute(&pool).await?;
        }
        assert!(super::super::conversions::tick(&pool, config.clone(), &media, &limits).await?);
        let published = wetube_backend::conversions::list(&pool).await?.remove(0);
        assert_eq!(
            published.state,
            wetube_core::ConversionState::Published,
            "{:?}",
            published.error
        );
        if reject_progress {
            assert!(published.progress.is_none());
        } else {
            let published_progress = published
                .progress
                .as_ref()
                .context("missing publication telemetry")?;
            assert_eq!(published_progress.stage, 6);
            assert_eq!(
                published_progress.phase,
                "Durably registering verified package"
            );
            assert_eq!(published_progress.total_units, None);
        }
        let package: String =
            sqlx::query_scalar("SELECT package_id FROM preview_media WHERE movie_id=?")
                .bind(published.movie_id.as_ref())
                .fetch_one(&pool)
                .await?;
        assert_eq!(
            std::fs::read(media.path().join(&package).join("movie.mp4"))?,
            movie_bytes
        );
        assert_eq!(
            std::fs::read(media.path().join(&package).join("original-tracks.mkv"))?,
            tracks_bytes
        );
        assert_eq!(
            std::fs::read(output_directory.join("movie.mp4"))?,
            movie_bytes
        );
        assert!(path.exists(), "generated input retained");
        super::super::conversions::recover(&pool).await?;
        assert!(!super::super::conversions::tick(&pool, config, &media, &limits).await?);
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM movies WHERE status='ready'")
            .fetch_one(&pool)
            .await?;
        assert_eq!(count, 1, "publication and completion commit exactly once");
        pool.close().await;
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
                None,
            )
            .await
            .is_err()
        );
        assert!(failed.path().join("movie.mp4").exists());
        Ok(())
    }
}
