use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use sqlx::{Row, SqlitePool};
use std::{
    fs::{File, OpenOptions},
    os::unix::fs::OpenOptionsExt,
    process::Stdio,
    sync::Arc,
    time::Duration,
};
use tokio::process::Command;
use wetube_core::{DvdScanReport, DvdTitle};

#[derive(Deserialize)]
#[serde(rename = "lsdvd")]
struct Inventory {
    #[serde(rename = "track", default)]
    tracks: Vec<Track>,
}

#[derive(Deserialize)]
struct Track {
    ix: u8,
    length: f64,
    angles: u8,
    #[serde(rename = "chapter", default)]
    chapters: Vec<Chapter>,
    #[serde(rename = "audio", default)]
    audio: Vec<Language>,
    #[serde(rename = "subp", default)]
    subtitles: Vec<Language>,
}

#[derive(Deserialize)]
struct Chapter {
    ix: u16,
}

#[derive(Deserialize)]
struct Language {
    #[serde(default)]
    langcode: Option<String>,
    #[serde(default)]
    language: Option<String>,
}

fn languages(streams: &[Language]) -> Result<Vec<String>> {
    let mut languages = Vec::new();
    for stream in streams {
        if let Some(language) = stream
            .langcode
            .as_ref()
            .filter(|text| !text.is_empty())
            .or(stream.language.as_ref())
        {
            ensure!(language.len() <= 128, "invalid DVD language metadata");
            if !language.is_empty() {
                languages.push(language.clone());
            }
        }
    }
    Ok(languages)
}

pub(crate) fn parse_inventory(bytes: &[u8]) -> Result<DvdScanReport> {
    ensure!(
        bytes.len() <= 2 * 1024 * 1024,
        "DVD inventory exceeds output quota"
    );
    let text = std::str::from_utf8(bytes)?;
    // libdvdread may print diagnostics before lsdvd's XML. The complete raw
    // output stays in the attempt log; only the XML document is deserialized.
    let start = text
        .find("<lsdvd>")
        .context("lsdvd returned no DVD XML inventory")?;
    let inventory: Inventory =
        quick_xml::de::from_str(&text[start..]).context("parse lsdvd XML")?;
    ensure!(
        !inventory.tracks.is_empty() && inventory.tracks.len() <= 99,
        "DVD has no valid bounded title inventory"
    );
    let mut titles = Vec::new();
    for track in inventory.tracks {
        ensure!(
            (1..=99).contains(&track.ix)
                && (1..=9).contains(&track.angles)
                && track.length.is_finite()
                && track.length > 0.0
                && track.length <= 86400.0,
            "invalid DVD title duration/angle inventory"
        );
        ensure!(
            !titles
                .iter()
                .any(|title: &DvdTitle| title.title == track.ix),
            "duplicate DVD title"
        );
        ensure!(
            track.chapters.len() <= 999 && track.audio.len() <= 8 && track.subtitles.len() <= 32,
            "DVD inventory exceeds chapter/track limits"
        );
        ensure!(
            track.chapters.iter().all(|chapter| chapter.ix > 0),
            "invalid chapter inventory"
        );
        titles.push(DvdTitle {
            title: track.ix,
            duration_seconds: track.length,
            chapter_count: track.chapters.len() as u16,
            audio_count: track.audio.len() as u16,
            subtitle_count: track.subtitles.len() as u16,
            audio_languages: languages(&track.audio)?,
            subtitle_languages: languages(&track.subtitles)?,
            angles: (1..=track.angles).collect(),
            main_feature_candidate: false,
        });
    }
    let longest = titles
        .iter()
        .map(|title| title.duration_seconds)
        .fold(0.0, f64::max);
    for title in &mut titles {
        title.main_feature_candidate = title.duration_seconds == longest;
    }
    titles.sort_by_key(|title| title.title);
    Ok(DvdScanReport { titles })
}

pub async fn recover(pool: &SqlitePool) -> Result<()> {
    sqlx::query("UPDATE scan_jobs SET state='failed',error='Worker interrupted. Staged input and logs retained; no automatic retry.' WHERE state IN ('staging','scanning')")
        .execute(pool).await?;
    Ok(())
}

fn integrated_command() -> Command {
    // Keep the running image pinned even when a build replaces its pathname.
    let mut command = Command::new("/proc/self/exe");
    command.arg("scan-metadata");
    command
}

pub async fn tick(pool: &SqlitePool, config: Arc<super::imports::ImportConfig>) -> Result<bool> {
    let attempt = uuid::Uuid::new_v4().to_string();
    let row = sqlx::query("UPDATE scan_jobs SET state='staging',attempt_id=? WHERE id=(SELECT id FROM scan_jobs WHERE state='queued' ORDER BY created_at,rowid LIMIT 1) RETURNING *")
        .bind(&attempt).fetch_optional(pool).await?;
    let Some(row) = row else {
        return Ok(false);
    };
    let id: String = row.try_get("id")?;
    let name: String = row.try_get("name")?;
    let expected: String = row.try_get("source_identity")?;
    let stage_config = config.clone();
    let stage_attempt = attempt.clone();
    let recording = super::progress::Recording::start(
        pool,
        "scan_jobs",
        &id,
        wetube_core::MediaWorkflow::Scan,
        0,
    );
    let progress = &recording.progress;
    let staging_progress = progress.clone();
    let result: Result<DvdScanReport> = async {
        let staged = tokio::task::spawn_blocking(move || {
            super::imports::stage_progress(
                &stage_config,
                &name,
                &expected,
                &stage_attempt,
                Some(&staging_progress),
            )
        })
        .await??;
        sqlx::query("UPDATE scan_jobs SET state='scanning',sha256=?,size_bytes=? WHERE id=?")
            .bind(&staged.sha256)
            .bind(staged.size_bytes as i64)
            .bind(&id)
            .execute(pool)
            .await?;
        staged.verify()?;
        progress.stage(2, "Discovering DVD titles and angles", None, "");
        config.require_capacity(4 * 1024 * 1024)?;
        let directory = config.scratch_path().join(&attempt);
        let stdout = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(directory.join("scan.stdout.txt"))?;
        let stderr = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(directory.join("scan.stderr.txt"))?;
        let integrated = super::dvdnav::available().is_ok();
        let mut command = if integrated {
            integrated_command()
        } else {
            let mut command = Command::new("lsdvd");
            command.args(["-Ox", "-x", "/proc/self/fd/0"]);
            command
        };
        let output = super::process::run_logged(
            command
                .env("DVDCSS_CACHE", "off")
                .stdin(Stdio::from(staged.file.try_clone()?)),
            Duration::from_secs(120),
            2 * 1024 * 1024,
            stdout.try_clone()?,
            stderr.try_clone()?,
        )
        .await
        .context(
            "DVD discovery failed; integrated libdvdnav or optional lsdvd fallback is required",
        )?;
        stdout.sync_all()?;
        stderr.sync_all()?;
        File::open(&directory)?.sync_all()?;
        staged.verify()?;
        ensure!(
            output.status.success(),
            "DVD discovery failed: {}",
            String::from_utf8_lossy(&output.stderr[..output.stderr.len().min(4096)])
        );
        if integrated {
            let text = std::str::from_utf8(&output.stdout)?;
            let (_, json) = text
                .rsplit_once("\nWETUBE_DVD_SCAN_JSON\n")
                .context("integrated scanner returned no inventory")?;
            serde_json::from_str(json.trim()).context("invalid integrated DVD inventory")
        } else {
            parse_inventory(&output.stdout)
        }
    }
    .await;
    match result {
        Ok(report) => {
            sqlx::query("UPDATE scan_jobs SET state='needs_review',report=?,error=NULL WHERE id=?")
                .bind(serde_json::to_string(&report)?)
                .bind(id)
                .execute(pool)
                .await?;
        }
        Err(error) => {
            sqlx::query("UPDATE scan_jobs SET state='failed',error=? WHERE id=?")
                .bind(format!("{error:#}").chars().take(4096).collect::<String>())
                .bind(id)
                .execute(pool)
                .await?;
        }
    }
    recording.finish().await;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn native_scanner_reexec_survives_unlinked_worker_image() -> Result<()> {
        let directory = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR"))?;
        let image = directory.path().join("worker-image");
        std::fs::copy("/proc/self/exe", &image)?;
        let output = super::super::process::run(
            Command::new(&image)
                .args([
                    "--ignored",
                    "--exact",
                    "scans::tests::unlinked_worker_image_child",
                ])
                .env("WETUBE_TEST_REEXEC_IMAGE", &image),
            Duration::from_secs(10),
            64 * 1024,
        )
        .await?;
        ensure!(
            output.status.success(),
            "unlinked worker could not launch its scanner: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!image.exists());
        Ok(())
    }

    #[tokio::test]
    #[ignore = "subprocess fixture for executable replacement"]
    async fn unlinked_worker_image_child() -> Result<()> {
        let image = std::path::PathBuf::from(std::env::var("WETUBE_TEST_REEXEC_IMAGE")?);
        ensure!(
            std::env::current_exe()? == image,
            "only unlink this disposable fixture's own image"
        );
        std::fs::remove_file(&image)?;
        // The copied test executable accepts scan-metadata as an empty test
        // filter. This exercises the actual launcher without a DVD or extra binary.
        let output = super::super::process::run(
            &mut integrated_command(),
            Duration::from_secs(5),
            64 * 1024,
        )
        .await?;
        ensure!(
            output.status.success(),
            "mapped scanner image was not executable"
        );
        Ok(())
    }

    #[test]
    fn inventory_exposes_actual_angles_tracks_and_longest_candidate_without_guessing_names() {
        let report = parse_inventory(br#"<lsdvd><track><ix>1</ix><length>30.5</length><angles>1</angles><chapter><ix>1</ix></chapter><audio><langcode>en</langcode></audio><subp><language>French</language></subp></track><track><ix>4</ix><length>1800</length><angles>3</angles></track></lsdvd>"#).unwrap();
        assert_eq!(report.titles[0].angles, vec![1]);
        assert_eq!(report.titles[1].angles, vec![1, 2, 3]);
        assert_eq!(report.titles[0].audio_languages, vec!["en"]);
        assert_eq!(report.titles[0].subtitle_languages, vec!["French"]);
        assert_eq!(report.titles[0].chapter_count, 1);
        assert!(!report.titles[0].main_feature_candidate);
        assert!(report.titles[1].main_feature_candidate);
        for bad in [
            "<lsdvd></lsdvd>",
            "<lsdvd><track><ix>1</ix><length>10</length></track></lsdvd>",
            "<lsdvd><track><ix>1</ix><length>NaN</length><angles>1</angles></track></lsdvd>",
            "<lsdvd><track><ix>1</ix><length>10</length><angles>10</angles></track></lsdvd>",
        ] {
            assert!(parse_inventory(bad.as_bytes()).is_err());
        }
    }
}
