//! Filesystem boundary for a separate local preview media root.
use anyhow::{Context, Result, ensure};
use std::{
    ffi::CString,
    fs::{File, OpenOptions},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::OpenOptionsExt,
    },
    path::{Path, PathBuf},
};

pub struct PreviewRoot {
    directory: File,
    path: PathBuf,
}

fn open_at(directory: &File, name: &str, directory_only: bool) -> Result<File> {
    let name = CString::new(name)?;
    let flags = libc::O_RDONLY
        | libc::O_NOFOLLOW
        | libc::O_CLOEXEC
        | libc::O_NONBLOCK
        | if directory_only { libc::O_DIRECTORY } else { 0 };
    let fd = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags) };
    ensure!(
        fd >= 0,
        "open preview entry: {}",
        std::io::Error::last_os_error()
    );
    Ok(unsafe { File::from_raw_fd(fd) })
}

impl PreviewRoot {
    pub fn open(path: &Path) -> Result<Self> {
        let directory = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(path)
            .context("open separate preview media root")?;
        let path = std::fs::canonicalize(format!("/proc/self/fd/{}", directory.as_raw_fd()))?;
        Ok(Self { directory, path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn descriptor_path(&self) -> PathBuf {
        PathBuf::from(format!("/proc/self/fd/{}", self.directory.as_raw_fd()))
    }

    pub fn require_separate(&self, other: &Path) -> Result<()> {
        let other = std::fs::canonicalize(other)?;
        ensure!(
            !self.path.starts_with(&other) && !other.starts_with(&self.path),
            "preview media must not overlap state, static, incoming, or experimental attempt roots"
        );
        Ok(())
    }

    pub fn open_movie(&self, package: &str) -> Result<File> {
        self.open_clip(package, "movie.mp4")
    }

    pub fn open_comparison(&self, package: &str, treatment: &str) -> Result<File> {
        let name = match treatment {
            "preserve" => "preserve.mp4",
            "yadif" => "yadif.mp4",
            _ => anyhow::bail!("invalid comparison treatment"),
        };
        self.open_clip(package, name)
    }

    fn open_clip(&self, package: &str, name: &str) -> Result<File> {
        let id = uuid::Uuid::parse_str(package)?;
        ensure!(id.to_string() == package, "invalid preview package ID");
        let directory = open_at(&self.directory, package, true)?;
        let file = open_at(&directory, name, false)?;
        ensure!(
            file.metadata()?.is_file() && file.metadata()?.len() > 0,
            "invalid preview movie"
        );
        Ok(file)
    }

    pub fn reserve_package(&self, package: &str) -> Result<File> {
        ensure!(
            uuid::Uuid::parse_str(package)?.to_string() == package,
            "invalid package ID"
        );
        let name = CString::new(package)?;
        ensure!(
            unsafe { libc::mkdirat(self.directory.as_raw_fd(), name.as_ptr(), 0o700) } == 0,
            "reserve package without overwrite: {}",
            std::io::Error::last_os_error()
        );
        self.directory.sync_all()?;
        open_at(&self.directory, package, true)
    }

    pub fn sync(&self) -> Result<()> {
        self.directory.sync_all().context("sync media root")
    }

    pub fn finalize(&self, staging: &str, package: &str) -> Result<()> {
        ensure!(
            uuid::Uuid::parse_str(package)?.to_string() == package
                && staging == format!(".admitting-{package}"),
            "invalid attempt-local preview finalization"
        );
        self.directory.sync_all()?;
        let source = CString::new(staging)?;
        let target = CString::new(package)?;
        let result = unsafe {
            libc::renameat2(
                self.directory.as_raw_fd(),
                source.as_ptr(),
                self.directory.as_raw_fd(),
                target.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        ensure!(
            result == 0,
            "finalize preview without overwrite: {}",
            std::io::Error::last_os_error()
        );
        self.directory.sync_all()?;
        Ok(())
    }
}
