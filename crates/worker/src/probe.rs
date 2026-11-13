use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    fs::{File, Metadata, OpenOptions},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::Path,
    process::Stdio,
    time::Duration,
};
use tokio::process::Command;

#[derive(Debug, Eq, PartialEq)]
struct Identity {
    device: u64,
    inode: u64,
    length: u64,
    modified: (i64, i64),
    changed: (i64, i64),
}

impl From<Metadata> for Identity {
    fn from(metadata: Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            length: metadata.len(),
            modified: (metadata.mtime(), metadata.mtime_nsec()),
            changed: (metadata.ctime(), metadata.ctime_nsec()),
        }
    }
}

fn open_iso(path: &Path) -> Result<(File, Identity)> {
    ensure!(
        path.extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("iso")),
        "select a completed .iso copy"
    );
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .context("open ISO read-only; symlink inputs are not accepted")?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file() && metadata.len() > 0,
        "ISO must be a non-empty regular file"
    );
    Ok((file, metadata.into()))
}

fn unchanged(path: &Path, file: &File, identity: &Identity) -> Result<()> {
    ensure!(
        Identity::from(file.metadata()?) == *identity
            && Identity::from(std::fs::symlink_metadata(path)?) == *identity,
        "ISO changed or was replaced during inspection; discard this report and recopy the input"
    );
    Ok(())
}

fn parse_metadata(bytes: &[u8]) -> Result<Value> {
    let metadata: Value = serde_json::from_slice(bytes).context("invalid ffprobe JSON")?;
    ensure!(
        metadata["format"]["format_name"].as_str() == Some("dvdvideo"),
        "ffprobe did not identify a DVD-Video input"
    );
    ensure!(
        metadata["streams"].as_array().is_some_and(|streams| streams
            .iter()
            .any(|stream| stream["codec_type"].as_str() == Some("video"))),
        "selected DVD title has no video stream"
    );
    Ok(metadata)
}

/// Inspect one operator-selected title; this is not the staged import protocol.
pub async fn inspect_dvd(path: &Path, title: u8, angle: u8) -> Result<Value> {
    let (file, _) = open_iso(path)?;
    inspect_open_iso(path, &file, title, angle).await
}

pub(crate) async fn inspect_open_iso(
    path: &Path,
    file: &File,
    title: u8,
    angle: u8,
) -> Result<Value> {
    ensure!(
        (1..=99).contains(&title),
        "DVD title must be between 1 and 99"
    );
    ensure!(
        (1..=9).contains(&angle),
        "DVD angle must be between 1 and 9"
    );
    let identity = Identity::from(file.metadata()?);
    let output = super::process::run(
        Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-probesize",
                "5000000",
                "-analyzeduration",
                "10000000",
                "-f",
                "dvdvideo",
                "-title",
                &title.to_string(),
                "-angle",
                &angle.to_string(),
                "-preindex",
                "0",
                "-show_format",
                "-show_streams",
                "-show_chapters",
                "-of",
                "json",
                "-i",
                "/proc/self/fd/0",
            ])
            // A seekable, read-only descriptor pins the input even if its pathname is replaced.
            .stdin(Stdio::from(file.try_clone()?)),
        Duration::from_secs(120),
        1024 * 1024,
    )
    .await
    .context("run ffprobe DVD inspection")?;
    unchanged(path, file, &identity)?;
    ensure!(
        output.status.success(),
        "DVD inspection failed: {}",
        String::from_utf8_lossy(&output.stderr[..output.stderr.len().min(4096)])
    );
    Ok(json!({
        "title": title,
        "angle": angle,
        "source_size_bytes": identity.length,
        "tool_messages": String::from_utf8_lossy(&output.stderr),
        "warnings": [
            "Read-only feasibility inspection, not authorization to import or delete.",
            "Only the selected title/angle was inspected; main-movie selection is not automatic.",
            "Chapter timings may be approximate because full-disc preindexing is disabled.",
            "Source metadata checks do not prove copy completion or exclude concurrent SMB writers."
        ],
        "metadata": parse_metadata(&output.stdout)?,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_iso_empty_and_symlink_inputs() {
        let dir = tempfile::tempdir().unwrap();
        let iso = dir.path().join("movie.iso");
        std::fs::write(&iso, b"").unwrap();
        assert!(open_iso(&iso).is_err());
        std::fs::write(&iso, b"not a DVD").unwrap();
        assert!(open_iso(&iso).is_ok());
        let link = dir.path().join("link.iso");
        std::os::unix::fs::symlink(&iso, &link).unwrap();
        assert!(open_iso(&link).is_err());
        assert!(open_iso(dir.path()).is_err());
    }

    #[test]
    fn rejects_modified_and_replaced_inputs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("movie.iso");
        std::fs::write(&path, b"original").unwrap();
        let (file, identity) = open_iso(&path).unwrap();
        unchanged(&path, &file, &identity).unwrap();
        std::fs::write(&path, b"modified and extended").unwrap();
        assert!(unchanged(&path, &file, &identity).is_err());
        let (file, identity) = open_iso(&path).unwrap();
        std::fs::rename(&path, dir.path().join("old.iso")).unwrap();
        std::fs::write(&path, b"modified and extended").unwrap();
        assert!(unchanged(&path, &file, &identity).is_err());
    }

    #[test]
    fn retains_track_and_chapter_metadata_but_requires_dvd_video() {
        let raw = br#"{"format":{"format_name":"dvdvideo"},"streams":[
            {"codec_type":"video","field_order":"tt"},
            {"codec_type":"audio","channels":6,"tags":{"language":"eng"}},
            {"codec_type":"subtitle","codec_name":"dvd_subtitle"}],
            "chapters":[{"start_time":"0.0","end_time":"60.0"}]}"#;
        let metadata = parse_metadata(raw).unwrap();
        assert_eq!(metadata["streams"][1]["channels"], 6);
        assert_eq!(metadata["chapters"][0]["end_time"], "60.0");
        assert!(parse_metadata(br#"{"streams":[]}"#).is_err());
        assert!(parse_metadata(br#"{"format":{"format_name":"dvdvideo"},"streams":[]}"#).is_err());
        assert!(parse_metadata(b"invalid JSON").is_err());
    }
}
