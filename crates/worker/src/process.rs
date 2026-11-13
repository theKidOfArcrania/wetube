use anyhow::{Context, Result};
use std::{process::ExitStatus, time::Duration};
use tokio::{io::AsyncReadExt, process::Command};

pub(crate) struct Output {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub status: ExitStatus,
}

async fn limited_output(
    reader: impl tokio::io::AsyncRead + Unpin,
    limit: usize,
) -> std::io::Result<Vec<u8>> {
    let mut output = Vec::new();
    reader
        .take((limit + 1) as u64)
        .read_to_end(&mut output)
        .await?;
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
            limited_output(stdout, output_limit),
            limited_output(stderr, output_limit),
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
        assert!(limited_output(&b"12345"[..], 4).await.is_err());
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
        std::fs::write(
            std::env::var_os("WETUBE_PROCESS_TEST_PID").expect("parent supplies PID path"),
            std::process::id().to_string(),
        )
        .unwrap();
        std::thread::sleep(Duration::from_secs(10));
    }
}
