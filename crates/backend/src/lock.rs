use anyhow::{Context, Result};
use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

pub struct MaintenanceLock {
    _file: File,
}

impl MaintenanceLock {
    pub fn acquire(state_dir: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(state_dir.join("maintenance.lock"))
            .context("open persistent maintenance lock")?;
        fs2::FileExt::try_lock_exclusive(&file)
            .context("another worker or maintenance command owns the state directory")?;

        // Children must keep ownership if their worker dies while they still run.
        let result = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFD, 0) };
        if result == -1 {
            return Err(std::io::Error::last_os_error()).context("inherit maintenance lock");
        }
        Ok(Self { _file: file })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A spawned fixture inherits every live maintenance descriptor in this process.
    static LOCK_TESTS: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn excludes_other_owners_until_dropped() {
        let _guard = LOCK_TESTS.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let first = MaintenanceLock::acquire(dir.path()).unwrap();
        assert!(MaintenanceLock::acquire(dir.path()).is_err());
        drop(first);
        assert!(MaintenanceLock::acquire(dir.path()).is_ok());
    }

    #[test]
    fn refuses_symlink_lock_files() {
        let _guard = LOCK_TESTS.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        std::fs::write(&target, b"").unwrap();
        std::os::unix::fs::symlink(target, dir.path().join("maintenance.lock")).unwrap();
        assert!(MaintenanceLock::acquire(dir.path()).is_err());
    }

    #[test]
    fn ownership_survives_parent_descriptor_drop_until_child_exits() {
        let _guard = LOCK_TESTS.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let lock = MaintenanceLock::acquire(dir.path()).unwrap();
        let ready = dir.path().join("child-ready");
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--ignored", "--exact", "lock::tests::inherited_lock_child"])
            .env("WETUBE_TEST_LOCK_READY", &ready)
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        for _ in 0..100 {
            if ready.exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(ready.exists(), "child did not start");
        drop(lock);
        assert!(MaintenanceLock::acquire(dir.path()).is_err());
        assert!(child.wait().unwrap().success());
        assert!(MaintenanceLock::acquire(dir.path()).is_ok());
    }

    #[test]
    #[ignore = "subprocess fixture for descriptor inheritance"]
    fn inherited_lock_child() {
        let ready = std::env::var_os("WETUBE_TEST_LOCK_READY").expect("parent supplies ready path");
        std::fs::write(ready, b"ready").unwrap();
        std::thread::sleep(std::time::Duration::from_secs(2));
    }
}
