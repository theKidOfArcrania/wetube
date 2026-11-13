use std::process::Command;

#[test]
fn invalid_dvd_fails_without_emitting_a_report_or_changing_the_input() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("incomplete.iso");
    let contents = b"this is an incomplete DVD copy";
    std::fs::write(&path, contents).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_wetube-worker"))
        .args(["inspect-dvd", "--title", "1", "--copy-complete", "--iso"])
        .arg(&path)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(
        error.contains("DVD inspection failed") || error.contains("run ffprobe DVD inspection"),
        "{error}"
    );
    assert_eq!(std::fs::read(path).unwrap(), contents);
}

#[tokio::test]
async fn configured_worker_persists_staged_failure_and_keeps_both_copies() {
    use wetube_backend::{db, incoming::IncomingRoot, probe_jobs};
    use wetube_core::{ProbeRequest, ProbeState};
    let state = tempfile::tempdir().unwrap();
    let incoming = tempfile::tempdir().unwrap();
    let scratch = state.path().join("scratch");
    let contents = b"completed but invalid DVD copy";
    std::fs::write(incoming.path().join("movie.iso"), contents).unwrap();
    db::initialize(state.path()).await.unwrap();
    let pool = db::connect(state.path()).await.unwrap();
    sqlx::query("INSERT INTO users VALUES ('admin', 'admin', 'test hash', 'admin')")
        .execute(&pool)
        .await
        .unwrap();
    let file = IncomingRoot::open(incoming.path())
        .unwrap()
        .list()
        .unwrap()
        .remove(0);
    probe_jobs::enqueue(
        &pool,
        "admin",
        &ProbeRequest {
            name: file.name,
            identity: file.identity,
            title: 1,
            angle: 1,
            copy_complete: true,
        },
    )
    .await
    .unwrap()
    .unwrap();
    let disabled = Command::new(env!("CARGO_BIN_EXE_wetube-worker"))
        .arg("--state-dir")
        .arg(state.path())
        .args(["run", "--once"])
        .output()
        .unwrap();
    assert!(disabled.status.success());
    assert_eq!(
        probe_jobs::list(&pool).await.unwrap()[0].state,
        ProbeState::Queued
    );
    assert!(!scratch.exists());
    let output = Command::new(env!("CARGO_BIN_EXE_wetube-worker"))
        .arg("--state-dir")
        .arg(state.path())
        .args(["run", "--once", "--incoming-dir"])
        .arg(incoming.path())
        .arg("--scratch-dir")
        .arg(&scratch)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let job = probe_jobs::list(&pool).await.unwrap().remove(0);
    assert_eq!(job.state, ProbeState::Failed);
    assert!(job.report.unwrap()["sha256"].is_string());
    assert_eq!(
        std::fs::read(scratch.join(job.attempt_id.unwrap()).join("input.iso")).unwrap(),
        contents
    );
    assert_eq!(
        std::fs::read(incoming.path().join("movie.iso")).unwrap(),
        contents
    );
    pool.close().await;
}
