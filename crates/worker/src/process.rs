use anyhow::{Context, Result};
use std::{fs::File, io::Write, process::ExitStatus, time::Duration};
use tokio::{io::AsyncReadExt, process::Command};

pub(crate) struct Output {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub status: ExitStatus,
}

async fn limited_output(
    reader: impl tokio::io::AsyncRead + Unpin,
    limit: usize,
    mut log: Option<File>,
) -> std::io::Result<Vec<u8>> {
    let mut output = Vec::new();
    let mut reader = reader.take((limit + 1) as u64);
    let mut buffer = [0u8; 8192];
    loop {
        let bytes = reader.read(&mut buffer).await?;
        if bytes == 0 {
            break;
        }
        if let Some(log) = &mut log {
            log.write_all(&buffer[..bytes])?;
        }
        output.extend_from_slice(&buffer[..bytes]);
    }
    if output.len() > limit {
        return Err(std::io::Error::other("tool output exceeded its byte limit"));
    }
    Ok(output)
}

struct ProcessGroup(u32);

impl Drop for ProcessGroup {
    fn drop(&mut self) {
        // Only this invocation's private process group is terminated.
        unsafe { libc::kill(-(self.0 as i32), libc::SIGKILL) };
    }
}

pub(crate) async fn run(
    command: &mut Command,
    timeout: Duration,
    output_limit: usize,
) -> Result<Output> {
    run_capture(command, timeout, output_limit, None, None).await
}

pub(crate) async fn run_logged(
    command: &mut Command,
    timeout: Duration,
    output_limit: usize,
    stdout_log: File,
    stderr_log: File,
) -> Result<Output> {
    run_capture(
        command,
        timeout,
        output_limit,
        Some(stdout_log),
        Some(stderr_log),
    )
    .await
}

async fn run_capture(
    command: &mut Command,
    timeout: Duration,
    output_limit: usize,
    stdout_log: Option<File>,
    stderr_log: Option<File>,
) -> Result<Output> {
    let mut child = command
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .process_group(0)
        .kill_on_drop(true)
        .spawn()
        .context("start media tool")?;
    let group = ProcessGroup(child.id().context("missing child process ID")?);
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let outcome = tokio::time::timeout(timeout, async {
        tokio::try_join!(
            limited_output(stdout, output_limit, stdout_log),
            limited_output(stderr, output_limit, stderr_log),
            child.wait()
        )
    })
    .await;
    match outcome {
        Ok(Ok((stdout, stderr, status))) => {
            std::mem::forget(group);
            Ok(Output {
                stdout,
                stderr,
                status,
            })
        }
        error => {
            drop(group);
            let _ = child.kill().await;
            let _ = child.wait().await;
            match error {
                Err(_) => anyhow::bail!("media tool timed out after {} seconds", timeout.as_secs()),
                Ok(Err(error)) => Err(error).context("read media tool output"),
                Ok(Ok(_)) => unreachable!(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn output_limits_and_deadlines_are_enforced() {
        assert!(limited_output(&b"12345"[..], 4, None).await.is_err());
        let captured = tempfile::NamedTempFile::new().unwrap();
        assert!(
            limited_output(&b"12345"[..], 4, Some(captured.reopen().unwrap()))
                .await
                .is_err()
        );
        assert_eq!(std::fs::read(captured.path()).unwrap(), b"12345");
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), vec![b'x'; 2048]).unwrap();
        assert!(
            run(
                Command::new("/bin/cat").arg(file.path()),
                Duration::from_secs(2),
                1024,
            )
            .await
            .err()
            .unwrap()
            .to_string()
            .contains("read media tool output")
        );
        let error = run(
            Command::new("/bin/sleep").arg("2"),
            Duration::from_millis(20),
            1024,
        )
        .await
        .err()
        .unwrap();
        assert!(error.to_string().contains("timed out"));
    }

    #[tokio::test]
    async fn timeout_retains_partial_stdout_and_stderr_logs() {
        let dir = tempfile::tempdir().unwrap();
        let stdout = tempfile::NamedTempFile::new().unwrap();
        let stderr = tempfile::NamedTempFile::new().unwrap();
        let error = run_logged(
            Command::new(std::env::current_exe().unwrap())
                .args([
                    "--ignored",
                    "--nocapture",
                    "--exact",
                    "process::tests::sleeping_child",
                ])
                .env("WETUBE_PROCESS_TEST_PID", dir.path().join("pid")),
            Duration::from_secs(1),
            1024,
            stdout.reopen().unwrap(),
            stderr.reopen().unwrap(),
        )
        .await
        .err()
        .unwrap();
        assert!(error.to_string().contains("timed out"));
        assert!(
            std::fs::read_to_string(stdout.path())
                .unwrap()
                .contains("fixture started")
        );
        assert!(
            std::fs::read_to_string(stderr.path())
                .unwrap()
                .contains("fixture waiting")
        );
    }

    #[tokio::test]
    async fn cancellation_terminates_the_running_tool() {
        let dir = tempfile::tempdir().unwrap();
        let ready = dir.path().join("child-pid");
        let ready_for_child = ready.clone();
        let task = tokio::spawn(async move {
            run(
                Command::new(std::env::current_exe().unwrap())
                    .args(["--ignored", "--exact", "process::tests::sleeping_child"])
                    .env("WETUBE_PROCESS_TEST_PID", ready_for_child),
                Duration::from_secs(20),
                1024,
            )
            .await
        });
        for _ in 0..200 {
            if ready.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let pid = std::fs::read_to_string(&ready).expect("fixture did not start");
        task.abort();
        assert!(matches!(task.await, Err(error) if error.is_cancelled()));
        let proc_path = std::path::PathBuf::from(format!("/proc/{pid}"));
        for _ in 0..200 {
            if !proc_path.exists() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("cancelled tool was not terminated and reaped");
    }

    #[test]
    #[ignore = "subprocess fixture for process cancellation"]
    fn sleeping_child() {
        println!("fixture started");
        std::io::stdout().flush().unwrap();
        eprintln!("fixture waiting");
        std::fs::write(
            std::env::var_os("WETUBE_PROCESS_TEST_PID").expect("parent supplies PID path"),
            std::process::id().to_string(),
        )
        .unwrap();
        std::thread::sleep(Duration::from_secs(10));
    }
}
