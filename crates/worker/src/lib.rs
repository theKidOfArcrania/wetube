use anyhow::{Context, Result};
use sqlx::SqlitePool;
use std::{process::Stdio, time::Duration};
use tokio::process::Command;
use wetube_backend::now;
use wetube_core::{DiagnosticReport, ToolCheck};

pub mod admission;
pub mod comparisons;
pub mod conversions;
pub mod dvdnav;
pub mod experimental;
pub mod imports;
pub mod preview;
pub mod probe;
mod process;
mod progress;
pub mod scans;
pub mod trash;

const OUTPUT_LIMIT: usize = 64 * 1024;

async fn tool_output(program: &str, args: &[&str]) -> Result<String> {
    let output = process::run(
        Command::new(program).args(args).stdin(Stdio::null()),
        Duration::from_secs(10),
        OUTPUT_LIMIT,
    )
    .await
    .with_context(|| format!("{program} diagnostic failed"))?;
    anyhow::ensure!(
        output.status.success(),
        "{program} returned a failure status"
    );
    Ok(format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    ))
}

pub fn has_dvdvideo_help(output: &str) -> bool {
    output
        .lines()
        .any(|line| line.starts_with("Demuxer dvdvideo "))
}

pub fn has_english_data(output: &str) -> bool {
    output.lines().any(|line| line.trim() == "eng")
}

pub async fn diagnose() -> DiagnosticReport {
    let mut tools = Vec::new();
    for (name, args) in [
        ("ffmpeg", vec!["-version"]),
        ("ffprobe", vec!["-version"]),
        ("lsdvd", vec!["-V"]),
        ("tesseract", vec!["--version"]),
    ] {
        let output = tool_output(name, &args).await;
        tools.push(ToolCheck {
            name: name.into(),
            available: output.is_ok(),
            detail: match output {
                Ok(output) => output
                    .lines()
                    .find(|line| !line.is_empty())
                    .unwrap_or("Available")
                    .into(),
                Err(error) => format!("{error:#}"),
            },
        });
    }
    let native = dvdnav::available();
    let fallback = tools
        .iter()
        .any(|tool| tool.name == "lsdvd" && tool.available);
    tools.push(ToolCheck {
        name: "DVD discovery".into(),
        available: native.is_ok() || fallback,
        detail: match native {
            Ok(()) => "Integrated libdvdnav title/chapter/angle inventory is available".into(),
            Err(_) if fallback => "Optional lsdvd inventory fallback is available".into(),
            Err(error) => {
                format!("DVD discovery unavailable: {error}; optional lsdvd fallback missing")
            }
        },
    });
    let mut dvdvideo_available = true;
    for program in ["ffmpeg", "ffprobe"] {
        dvdvideo_available &= tool_output(program, &["-hide_banner", "-h", "demuxer=dvdvideo"])
            .await
            .is_ok_and(|output| has_dvdvideo_help(&output));
    }
    tools.push(ToolCheck {
        name: "dvdvideo".into(),
        available: dvdvideo_available,
        detail: if dvdvideo_available {
            "FFmpeg and ffprobe include the DVD-Video demuxer"
        } else {
            "FFmpeg and ffprobe must both be built with DVD-Video support"
        }
        .into(),
    });
    let english_available = tool_output("tesseract", &["--list-langs"])
        .await
        .is_ok_and(|output| has_english_data(&output));
    tools.push(ToolCheck {
        name: "English OCR".into(),
        available: english_available,
        detail: if english_available {
            "English trained data is available"
        } else {
            "Install Tesseract English trained data before caption OCR"
        }
        .into(),
    });
    DiagnosticReport {
        tools,
        dvdvideo_available,
    }
}

pub async fn recover(pool: &SqlitePool) -> Result<()> {
    sqlx::query(
        "UPDATE diagnostic_jobs SET state = 'queued', started_at = NULL
         WHERE state = 'running'",
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn tick(pool: &SqlitePool) -> Result<bool> {
    heartbeat(pool).await?;
    let id: Option<String> = sqlx::query_scalar(
        "UPDATE diagnostic_jobs SET state = 'running', started_at = ?, error = NULL
         WHERE id = (SELECT id FROM diagnostic_jobs WHERE state = 'queued'
         ORDER BY created_at, rowid LIMIT 1) RETURNING id",
    )
    .bind(now())
    .fetch_optional(pool)
    .await?;
    let Some(id) = id else {
        return Ok(false);
    };
    let report = diagnose().await;
    let healthy = report.required_tools_available();
    sqlx::query(
        "UPDATE diagnostic_jobs SET state = ?, finished_at = ?, report = ?, error = ?
         WHERE id = ? AND state = 'running'",
    )
    .bind(if healthy { "succeeded" } else { "failed" })
    .bind(now())
    .bind(serde_json::to_string(&report)?)
    .bind(
        (!healthy).then_some(
            "One or more required scan/conversion tools are unavailable; OCR is optional",
        ),
    )
    .bind(id)
    .execute(pool)
    .await?;
    heartbeat(pool).await?;
    Ok(true)
}

pub async fn heartbeat(pool: &SqlitePool) -> Result<()> {
    sqlx::query(
        "INSERT INTO worker_status (singleton, last_seen) VALUES (1, ?)
         ON CONFLICT(singleton) DO UPDATE SET last_seen = excluded.last_seen",
    )
    .bind(now())
    .execute(pool)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_successful_exit_alone_does_not_prove_dvd_support() {
        assert!(has_dvdvideo_help("Demuxer dvdvideo [DVD-Video]:\noptions"));
        assert!(!has_dvdvideo_help("Unknown format 'dvdvideo'."));
        assert!(!has_dvdvideo_help("configuration: --enable-libdvdread"));
    }

    #[test]
    fn english_data_requires_the_exact_language_entry() {
        assert!(has_english_data(
            "List of available languages (2):\neng\nosd\n"
        ));
        assert!(!has_english_data("/a/path/containing/eng\nosd\n"));
    }

    #[tokio::test]
    async fn missing_tools_are_reported() {
        assert!(tool_output("/nonexistent-wetube-tool", &[]).await.is_err());
    }

    #[tokio::test]
    async fn recovery_requeues_only_interrupted_diagnostics() {
        let dir = tempfile::tempdir().unwrap();
        wetube_backend::db::initialize(dir.path()).await.unwrap();
        let pool = wetube_backend::db::connect(dir.path()).await.unwrap();
        sqlx::query("INSERT INTO users VALUES ('u', 'admin', 'unused', 'admin')")
            .execute(&pool)
            .await
            .unwrap();
        for (id, state) in [("interrupted", "running"), ("done", "succeeded")] {
            sqlx::query(
                "INSERT INTO diagnostic_jobs (id, requested_by, state, created_at) VALUES (?, 'u', ?, 1)",
            )
            .bind(id).bind(state).execute(&pool).await.unwrap();
        }
        recover(&pool).await.unwrap();
        let states: Vec<(String, String)> =
            sqlx::query_as("SELECT id, state FROM diagnostic_jobs ORDER BY id")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(
            states,
            vec![
                ("done".into(), "succeeded".into()),
                ("interrupted".into(), "queued".into())
            ]
        );
    }
}
