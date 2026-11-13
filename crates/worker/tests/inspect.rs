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
