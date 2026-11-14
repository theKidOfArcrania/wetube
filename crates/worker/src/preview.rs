//! Bounded validation of operator-admitted local previews; never converts or deletes sources.
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::{
    fs::File,
    io::{Seek, SeekFrom},
    process::Stdio,
    time::Duration,
};
use tokio::process::Command;

pub struct Validated {
    pub duration_seconds: f64,
    pub decoded_frames: u64,
}

async fn checked(command: &mut Command, timeout: Duration) -> Result<Vec<u8>> {
    let output = super::process::run(command, timeout, 2 * 1024 * 1024).await?;
    ensure!(
        output.status.success() && output.stderr.is_empty(),
        "preview media tool rejected payload: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(output.stdout)
}

pub async fn validate(file: &File, report: &Value, timeout: Duration) -> Result<Validated> {
    validate_progress(file, report, timeout, None).await
}

pub(crate) async fn validate_progress(
    file: &File,
    report: &Value,
    timeout: Duration,
    progress: Option<&crate::progress::Progress>,
) -> Result<Validated> {
    ensure!(
        timeout > Duration::ZERO && timeout <= Duration::from_secs(86400),
        "invalid validation deadline"
    );
    let mut input = file.try_clone()?;
    input.seek(SeekFrom::Start(0))?;
    let bytes = checked(
        Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-protocol_whitelist",
                "file,pipe",
                "-f",
                "mov",
                "-i",
                "/proc/self/fd/0",
                "-count_packets",
                "-show_format",
                "-show_streams",
                "-show_chapters",
                "-of",
                "json",
            ])
            .stdin(Stdio::from(input)),
        timeout,
    )
    .await?;
    let metadata: Value = serde_json::from_slice(&bytes)?;
    let duration: f64 = metadata["format"]["duration"]
        .as_str()
        .context("missing duration")?
        .parse()?;
    ensure!(duration.is_finite() && duration > 0.0, "invalid duration");
    ensure!(
        matches!(
            metadata["format"]["tags"]["major_brand"].as_str(),
            Some("isom" | "iso2" | "iso5" | "iso6" | "mp41" | "mp42" | "avc1" | "M4V ")
        ),
        "preview requires an MP4 container"
    );
    let actual = metadata["streams"].as_array().context("missing streams")?;
    let expected = report["validation"]["movie"]["streams"]
        .as_array()
        .context("missing validated streams")?;
    ensure!(
        !actual.is_empty() && actual.len() <= 16 && actual.len() == expected.len(),
        "stream count changed"
    );
    let mut videos = 0;
    for (a, e) in actual.iter().zip(expected) {
        ensure!(
            a["codec_type"] == e["codec_type"] && a["codec_name"] == e["codec_name"],
            "stream layout changed"
        );
        if matches!(a["codec_type"].as_str(), Some("video" | "audio")) {
            ensure!(
                a["nb_read_packets"]
                    .as_str()
                    .and_then(|n| n.parse::<u64>().ok())
                    .is_some_and(|n| n > 0),
                "encoded track has no packets"
            );
            if let Some(expected) = e.get("nb_read_packets") {
                ensure!(
                    &a["nb_read_packets"] == expected,
                    "encoded packet count changed"
                );
            }
        }
        match a["codec_type"].as_str() {
            Some("video") => {
                videos += 1;
                ensure!(
                    a["codec_name"] == "h264" && a["pix_fmt"] == "yuv420p",
                    "preview requires H264 yuv420p"
                );
                for key in ["width", "height", "pix_fmt"] {
                    ensure!(a[key] == e[key], "video metadata changed");
                }
                ensure!(
                    a["width"].as_u64().is_some_and(|n| n > 0 && n <= 8192)
                        && a["height"].as_u64().is_some_and(|n| n > 0 && n <= 8192),
                    "invalid video dimensions"
                );
            }
            Some("audio") => ensure!(
                a["codec_name"] == "aac" && a["channels"] == 2 && a["channels"] == e["channels"],
                "preview requires AAC stereo"
            ),
            // ffmpeg's preserved MP4 chapter track is data, not a playable subtitle track.
            Some("data") => ensure!(
                a["codec_name"] == "bin_data" && a["codec_tag_string"] == "text",
                "unsupported preview data track"
            ),
            _ => anyhow::bail!("unsupported preview stream"),
        }
    }
    ensure!(videos == 1, "expected one video stream");
    if let Some(chapters) = report["validation"]["movie"]["chapters"].as_array() {
        let actual = metadata["chapters"]
            .as_array()
            .context("missing MP4 chapters")?;
        ensure!(actual.len() == chapters.len(), "chapter count changed");
        for (actual, expected) in actual.iter().zip(chapters) {
            ensure!(
                actual["start_time"] == expected["start_time"]
                    && actual["end_time"] == expected["end_time"],
                "chapter timing changed"
            );
        }
    }
    let validation = &report["validation"];
    let expected_duration: f64 = validation["movie"]["format"]["duration"]
        .as_str()
        .context("missing report duration")?
        .parse()?;
    ensure!(
        expected_duration.is_finite()
            && expected_duration > 0.0
            && (duration - expected_duration).abs() <= 0.01,
        "MP4 duration differs from experimental report"
    );
    let source_frames = validation["source_decoded_frames"]
        .as_u64()
        .context("missing source frames")?;
    ensure!(
        source_frames > 0 && validation["output_decoded_frames"].as_u64() == Some(source_frames),
        "incomplete frame validation"
    );
    let source_seconds = validation["source_decoded_seconds"]
        .as_f64()
        .context("missing source decode duration")?;
    let output_seconds = validation["output_decoded_seconds"]
        .as_f64()
        .context("missing output decode duration")?;
    same_duration(source_seconds, output_seconds)?;
    same_duration(duration, output_seconds)?;
    let mut input = file.try_clone()?;
    input.seek(SeekFrom::Start(0))?;
    if let Some(progress) = progress {
        progress.stage(
            5,
            "Full decode of published movie",
            Some(source_seconds),
            "media seconds",
        );
    }
    let output = super::process::run_progress(
        Command::new("ffmpeg")
            .args([
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
                "2",
                "-protocol_whitelist",
                "file,pipe",
                "-f",
                "mov",
                "-i",
                "/proc/self/fd/0",
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
                "-stats_period",
                "1",
                "-f",
                "null",
                "-",
            ])
            .stdin(Stdio::from(input)),
        timeout,
        2 * 1024 * 1024,
        progress,
    )
    .await?;
    ensure!(
        output.status.success() && output.stderr.is_empty(),
        "preview media tool rejected payload: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let bytes = output.stdout;
    let text = std::str::from_utf8(&bytes)?;
    let mut frames = None;
    let mut seconds = None;
    let mut ended = false;
    for line in text.lines() {
        if let Some((key, value)) = line.split_once('=') {
            match key {
                "frame" => frames = value.trim().parse::<u64>().ok(),
                "out_time_us" => {
                    seconds = value
                        .trim()
                        .parse::<i64>()
                        .ok()
                        .map(|n| n as f64 / 1_000_000.0)
                }
                "progress" => ended = value == "end",
                _ => {}
            }
        }
    }
    ensure!(
        ended && frames == Some(source_frames),
        "full decoded frame count changed: {frames:?} versus {source_frames}; decode finished: {ended}"
    );
    same_duration(
        seconds.context("decode did not report duration")?,
        output_seconds,
    )?;
    Ok(Validated {
        duration_seconds: duration,
        decoded_frames: source_frames,
    })
}

fn same_duration(actual: f64, expected: f64) -> Result<()> {
    ensure!(
        actual.is_finite()
            && expected.is_finite()
            && actual > 0.0
            && expected > 0.0
            && (actual - expected).abs() <= 5.0 + expected * 0.001,
        "preview duration changed"
    );
    Ok(())
}
