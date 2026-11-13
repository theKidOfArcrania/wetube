use anyhow::{Context, Result};
use sqlx::SqlitePool;
use std::{process::Stdio, time::Duration};
use tokio::{io::AsyncReadExt, process::Command};
use wetube_backend::now;
use wetube_core::{DiagnosticReport, ToolCheck};

const OUTPUT_LIMIT: usize = 64 * 1024;

async fn limited_output(reader: impl tokio::io::AsyncRead + Unpin) -> std::io::Result<Vec<u8>> {
    let mut output = Vec::new();
    reader
        .take((OUTPUT_LIMIT + 1) as u64)
        .read_to_end(&mut output)
        .await?;
    if output.len() > OUTPUT_LIMIT {
        return Err(std::io::Error::other("tool output exceeded 64 KiB"));
    }
    Ok(output)
}

async fn tool_output(program: &str, args: &[&str]) -> Result<String> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("{program} is not available"))?;
    let id = child.id().context("missing child process ID")?;
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let outcome = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::try_join!(limited_output(stdout), limited_output(stderr), child.wait())
    })
    .await;
    match outcome {
        Ok(Ok((stdout, stderr, status))) => {
            anyhow::ensure!(status.success(), "{program} returned a failure status");
            Ok(format!(
                "{}\n{}",
                String::from_utf8_lossy(&stdout),
                String::from_utf8_lossy(&stderr)
            ))
        }
        error => {
            // This process group was created solely for this invocation.
            unsafe { libc::kill(-(id as i32), libc::SIGKILL) };
            let _ = child.kill().await;
            let _ = child.wait().await;
            match error {
                Err(_) => anyhow::bail!("{program} timed out after ten seconds"),
                Ok(Err(error)) => Err(error).context("read diagnostic tool output"),
                Ok(Ok(_)) => unreachable!(),
            }
        }
    }
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
                Err(error) => error.to_string(),
            },
        });
    }
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
    let healthy = report.tools.iter().all(|tool| tool.available);
    sqlx::query(
        "UPDATE diagnostic_jobs SET state = ?, finished_at = ?, report = ?, error = ?
         WHERE id = ? AND state = 'running'",
    )
    .bind(if healthy { "succeeded" } else { "failed" })
    .bind(now())
    .bind(serde_json::to_string(&report)?)
    .bind((!healthy).then_some("One or more media prerequisites are unavailable"))
    .bind(id)
    .execute(pool)
    .await?;
    heartbeat(pool).await?;
    Ok(true)
}

async fn heartbeat(pool: &SqlitePool) -> Result<()> {
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
    async fn output_is_bounded_and_missing_tools_are_reported() {
        assert!(
            limited_output(&vec![0; OUTPUT_LIMIT + 1][..])
                .await
                .is_err()
        );
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
