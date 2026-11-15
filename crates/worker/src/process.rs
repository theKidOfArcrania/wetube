use crate::progress::Progress;
use anyhow::{Context, Result};
use std::{fs::File, io::Write, process::ExitStatus, time::Duration};
use tokio::{io::AsyncReadExt, process::Command};

enum Capture<'a> {
    Bounded,
    Progress(Option<&'a Progress>),
}

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

fn subreaper() -> Result<()> {
    static RESULT: std::sync::OnceLock<Result<(), i32>> = std::sync::OnceLock::new();
    let result = RESULT.get_or_init(|| {
        if unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) } == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error()
                .raw_os_error()
                .unwrap_or(libc::EINVAL))
        }
    });
    match result {
        Ok(()) => Ok(()),
        Err(code) => Err(std::io::Error::from_raw_os_error(*code))
            .context("isolate owned subprocess reaping"),
    }
}

async fn exited_without_reaping(pid: u32) -> std::io::Result<()> {
    loop {
        let exited = {
            let mut info = unsafe { std::mem::zeroed::<libc::siginfo_t>() };
            let result = unsafe {
                libc::waitid(
                    libc::P_PID,
                    pid,
                    &mut info,
                    libc::WEXITED | libc::WNOWAIT | libc::WNOHANG,
                )
            };
            if result != 0 {
                return Err(std::io::Error::last_os_error());
            }
            (unsafe { info.si_pid() }) != 0
        };
        if exited {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn reap_group(pid: u32) -> std::io::Result<()> {
    loop {
        let result = unsafe { libc::waitpid(-(pid as i32), std::ptr::null_mut(), libc::WNOHANG) };
        if result > 0 {
            continue;
        }
        if result < 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ECHILD) {
                return Ok(());
            }
            if error.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(error);
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
pub(crate) async fn run(
    command: &mut Command,
    timeout: Duration,
    output_limit: usize,
) -> Result<Output> {
    run_capture(command, timeout, output_limit, None, None, Capture::Bounded).await
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
        Capture::Bounded,
    )
    .await
}

pub(crate) async fn run_logged_progress(
    command: &mut Command,
    timeout: Duration,
    output_limit: usize,
    stdout_log: File,
    stderr_log: File,
    progress: Option<&Progress>,
) -> Result<Output> {
    run_capture(
        command,
        timeout,
        output_limit,
        Some(stdout_log),
        Some(stderr_log),
        Capture::Progress(progress),
    )
    .await
}

pub(crate) async fn run_progress(
    command: &mut Command,
    timeout: Duration,
    output_limit: usize,
    progress: Option<&Progress>,
) -> Result<Output> {
    run_capture(
        command,
        timeout,
        output_limit,
        None,
        None,
        Capture::Progress(progress),
    )
    .await
}

async fn progress_output(
    mut reader: impl tokio::io::AsyncRead + Unpin,
    limit: usize,
    mut log: Option<File>,
    progress: Option<&Progress>,
) -> std::io::Result<Vec<u8>> {
    let mut output = Vec::new();
    let mut pending = Vec::new();
    let mut logged = 0;
    let mut buffer = [0u8; 8192];
    loop {
        let bytes = reader.read(&mut buffer).await?;
        if bytes == 0 {
            break;
        }
        let keep = bytes.min(limit.saturating_sub(logged));
        if let Some(log) = &mut log {
            log.write_all(&buffer[..keep])?;
        }
        logged += keep;
        output.extend_from_slice(&buffer[..bytes]);
        if output.len() > limit {
            output.drain(..output.len() - limit);
        }
        for byte in &buffer[..bytes] {
            if *byte == b'\n' {
                if let Some(progress) = progress
                    && let Ok(line) = std::str::from_utf8(&pending)
                {
                    progress.ffmpeg_line(line);
                }
                pending.clear();
            } else {
                if pending.len() >= 8192 {
                    return Err(std::io::Error::other("oversized FFmpeg progress line"));
                }
                pending.push(*byte);
            }
        }
    }
    Ok(output)
}

async fn run_capture(
    command: &mut Command,
    timeout: Duration,
    output_limit: usize,
    stdout_log: Option<File>,
    stderr_log: Option<File>,
    capture: Capture<'_>,
) -> Result<Output> {
    subreaper()?;
    let cancel = super::coordinator::cancellation();
    if let Some(cancel) = &cancel {
        cancel.check()?;
    }
    let mut child = command
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .process_group(0)
        .kill_on_drop(true)
        .spawn()
        .context("start media tool")?;
    let group = ProcessGroup(child.id().context("missing child process ID")?);
    let pid = group.0;
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let outcome = {
        let running = tokio::time::timeout(timeout, async {
            tokio::try_join!(
                async {
                    match capture {
                        Capture::Progress(progress) => {
                            progress_output(stdout, output_limit, stdout_log, progress).await
                        }
                        Capture::Bounded => limited_output(stdout, output_limit, stdout_log).await,
                    }
                },
                limited_output(stderr, output_limit, stderr_log),
                exited_without_reaping(pid)
            )
        });
        tokio::pin!(running);
        tokio::select! {
            outcome = &mut running => Some(outcome),
            _ = async {
                match &cancel {
                    Some(cancel)=>cancel.cancelled().await,
                    None=>std::future::pending::<()>().await,
                }
            } => None,
        }
    };
    match outcome {
        Some(Ok(Ok((stdout, stderr, ())))) => {
            // Keep the leader unreaped until its private group has been stopped:
            // its PID cannot be recycled into an unrelated process group.
            drop(group);
            let status = child.wait().await?;
            reap_group(pid).await?;
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
            reap_group(pid).await?;
            match error {
                None => anyhow::bail!("Cancelled by administrator; owned media tool stopped"),
                Some(Err(_)) => {
                    anyhow::bail!("media tool timed out after {} seconds", timeout.as_secs())
                }
                Some(Ok(Err(error))) => Err(error).context("read media tool output"),
                Some(Ok(Ok(_))) => unreachable!(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn streaming_progress_keeps_bounded_logs_and_the_final_validation_record() {
        let log = tempfile::NamedTempFile::new().unwrap();
        let stream = format!(
            "{}frame=100\nout_time_us=4000000\nprogress=end\n",
            "out_time_us=N/A\n".repeat(10000)
        );
        let captured = progress_output(stream.as_bytes(), 128, Some(log.reopen().unwrap()), None)
            .await
            .unwrap();
        assert!(captured.len() <= 128);
        assert!(
            std::str::from_utf8(&captured)
                .unwrap()
                .ends_with("frame=100\nout_time_us=4000000\nprogress=end\n")
        );
        assert_eq!(std::fs::metadata(log.path()).unwrap().len(), 128);
        assert!(
            progress_output(&vec![b'x'; 8193][..], 128, None, None)
                .await
                .is_err()
        );
    }

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
