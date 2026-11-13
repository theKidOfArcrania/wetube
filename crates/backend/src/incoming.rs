use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};
use std::{
    ffi::CString,
    fs::{File, Metadata, OpenOptions},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
};
use wetube_core::IncomingFile;

pub struct IncomingRoot {
    directory: File,
}

pub fn identity(metadata: &Metadata) -> String {
    let fields = format!(
        "{}:{}:{}:{}:{}:{}:{}",
        metadata.dev(),
        metadata.ino(),
        metadata.len(),
        metadata.mtime(),
        metadata.mtime_nsec(),
        metadata.ctime(),
        metadata.ctime_nsec()
    );
    hex::encode(Sha256::digest(fields.as_bytes()))
}

pub fn validate_name(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty()
            && name.len() <= 255
            && !name.contains(['/', '\\', '\0'])
            && Path::new(name)
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("iso")),
        "select a single ordinary .iso filename from the incoming root"
    );
    Ok(())
}

impl IncomingRoot {
    pub fn open(path: &Path) -> Result<Self> {
        let directory = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(path)
            .context("open incoming directory without following a symlink")?;
        Ok(Self { directory })
    }

    pub fn open_file(&self, name: &str) -> Result<(File, IncomingFile)> {
        validate_name(name)?;
        let c_name = CString::new(name)?;
        let fd = unsafe {
            libc::openat(
                self.directory.as_raw_fd(),
                c_name.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error()).context("open incoming ISO");
        }
        let file = unsafe { File::from_raw_fd(fd) };
        let metadata = file.metadata()?;
        ensure!(
            metadata.is_file() && metadata.len() > 0,
            "input must be a non-empty regular ISO"
        );
        let entry = IncomingFile {
            name: name.into(),
            size_bytes: metadata.len(),
            identity: identity(&metadata),
        };
        Ok((file, entry))
    }

    pub fn list(&self) -> Result<Vec<IncomingFile>> {
        let mut files = Vec::new();
        let directory = PathBuf::from(format!("/proc/self/fd/{}", self.directory.as_raw_fd()));
        for (index, entry) in std::fs::read_dir(directory)?.enumerate() {
            ensure!(
                index < 10_000,
                "incoming directory has too many entries; split the batch"
            );
            let entry = entry?;
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if validate_name(&name).is_err() || !entry.file_type()?.is_file() {
                continue;
            }
            // Incomplete, inaccessible, or concurrently replaced files are not candidates.
            if let Ok((_, entry)) = self.open_file(&name) {
                files.push(entry);
            }
            ensure!(
                files.len() <= 1000,
                "incoming directory has too many ISOs; split the batch"
            );
        }
        files.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(files)
    }

    pub fn verify(&self, name: &str, file: &File, expected: &str) -> Result<()> {
        ensure!(
            identity(&file.metadata()?) == expected && self.open_file(name)?.1.identity == expected,
            "source changed or was replaced during staging; input retained"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confines_names_and_skips_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("movie.ISO"), b"copy").unwrap();
        std::fs::write(dir.path().join("incomplete.iso"), b"").unwrap();
        std::fs::create_dir(dir.path().join("directory.iso")).unwrap();
        std::os::unix::fs::symlink("movie.ISO", dir.path().join("link.iso")).unwrap();
        let root = IncomingRoot::open(dir.path()).unwrap();
        assert_eq!(root.list().unwrap().len(), 1);
        for name in [
            "../movie.ISO",
            "/tmp/movie.iso",
            "sub/movie.iso",
            "sub\\movie.iso",
            "link.iso",
        ] {
            assert!(root.open_file(name).is_err());
        }
        let (file, before) = root.open_file("movie.ISO").unwrap();
        root.verify("movie.ISO", &file, &before.identity).unwrap();
        std::fs::write(dir.path().join("movie.ISO"), b"changed copy").unwrap();
        assert!(root.verify("movie.ISO", &file, &before.identity).is_err());
        let (_, after) = root.open_file("movie.ISO").unwrap();
        assert_ne!(before.identity, after.identity);
        let (file, before) = root.open_file("movie.ISO").unwrap();
        std::fs::rename(dir.path().join("movie.ISO"), dir.path().join("old.iso")).unwrap();
        std::fs::write(dir.path().join("movie.ISO"), b"changed copy").unwrap();
        assert!(root.verify("movie.ISO", &file, &before.identity).is_err());
    }
}
