//! Filesystem boundary for a separate local preview media root.
use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};
use std::{
    ffi::CString,
    fs::{File, OpenOptions},
    io::Read,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{
            ffi::OsStrExt,
            fs::{MetadataExt, OpenOptionsExt},
        },
    },
    path::{Path, PathBuf},
};

pub struct PreviewRoot {
    directory: File,
    path: PathBuf,
}

pub struct PurgeOutput<'a> {
    pub root_identity: &'a str,
    pub directory: &'a str,
    pub directory_identity: &'a str,
    pub name: &'a str,
    pub identity: &'a str,
    pub stable_identity: &'a str,
    pub sha256: &'a str,
}

fn open_at(directory: &File, name: &str, directory_only: bool) -> Result<File> {
    let name = CString::new(name)?;
    let flags = libc::O_RDONLY
        | libc::O_NOFOLLOW
        | libc::O_CLOEXEC
        | libc::O_NONBLOCK
        | if directory_only { libc::O_DIRECTORY } else { 0 };
    let fd = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error()).context("open preview entry");
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

pub fn stable_identity(metadata: &std::fs::Metadata) -> String {
    format!(
        "{}:{}:{}:{}:{}",
        metadata.dev(),
        metadata.ino(),
        metadata.len(),
        metadata.mtime(),
        metadata.mtime_nsec()
    )
}

fn directory_identity(directory: &File) -> Result<String> {
    let m = directory.metadata()?;
    Ok(format!("{}:{}", m.dev(), m.ino()))
}

fn optional_at(directory: &File, name: &str, directory_only: bool) -> Result<Option<File>> {
    match open_at(directory, name, directory_only) {
        Ok(file) => Ok(Some(file)),
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

impl PreviewRoot {
    pub fn open(path: &Path) -> Result<Self> {
        let directory = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(path)
            .context("open separate preview media root")?;
        Self::from_directory(directory)
    }

    pub fn from_directory(directory: File) -> Result<Self> {
        ensure!(
            directory.metadata()?.is_dir(),
            "expected pinned media directory"
        );
        let path = std::fs::canonicalize(format!("/proc/self/fd/{}", directory.as_raw_fd()))?;
        Ok(Self { directory, path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn matches_root_path(&self, recorded: &[u8]) -> bool {
        self.path.as_os_str().as_bytes() == recorded
    }

    pub fn verify_root_identity(&self, recorded: &str) -> Result<()> {
        ensure!(
            directory_identity(&self.directory)? == recorded,
            "asset root was replaced"
        );
        Ok(())
    }

    pub fn descriptor_path(&self) -> PathBuf {
        PathBuf::from(format!("/proc/self/fd/{}", self.directory.as_raw_fd()))
    }

    pub fn try_clone(&self) -> Result<Self> {
        Ok(Self {
            directory: self.directory.try_clone()?,
            path: self.path.clone(),
        })
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

    pub fn open_asset(&self, package: &str, name: &str) -> Result<File> {
        ensure!(
            matches!(
                name,
                "movie.mp4" | "original-tracks.mkv" | "manifest.json" | "manifest.sha256"
            ),
            "not a generated asset"
        );
        self.open_clip(package, name)
    }

    pub fn inspect_outputs(&self, package: &str) -> Result<Option<Vec<(String, String)>>> {
        ensure!(
            uuid::Uuid::parse_str(package)?.to_string() == package,
            "invalid asset directory"
        );
        let Some(directory) = optional_at(&self.directory, package, true)? else {
            return Ok(None);
        };
        let mut outputs = Vec::new();
        for name in ["movie.mp4", "original-tracks.mkv"] {
            if let Some(file) = optional_at(&directory, name, false)? {
                let metadata = file.metadata()?;
                ensure!(
                    metadata.is_file() && metadata.nlink() == 1 && metadata.len() > 0,
                    "generated asset must be a nonempty unshared regular file"
                );
                outputs.push((name.to_owned(), crate::incoming::identity(&metadata)));
            }
        }
        Ok(Some(outputs))
    }

    pub async fn record_verified_output(
        &self,
        connection: &mut sqlx::SqliteConnection,
        movie: &str,
        root: &str,
        package: &str,
        name: &str,
        attestation: (&str, &str, bool),
    ) -> Result<()> {
        ensure!(
            uuid::Uuid::parse_str(package)?.to_string() == package
                && matches!(name, "movie.mp4" | "original-tracks.mkv"),
            "invalid generated asset"
        );
        let directory = open_at(&self.directory, package, true)?;
        let metadata = open_at(&directory, name, false)?.metadata()?;
        ensure!(
            metadata.is_file()
                && metadata.nlink() == 1
                && crate::incoming::identity(&metadata) == attestation.0,
            "validated generated asset was replaced"
        );
        sqlx::query("INSERT INTO movie_outputs(movie_id,root,root_path,root_identity,directory,directory_identity,name,identity,stable_identity,sha256,origin) VALUES(?,?,?,?,?,?,?,?,?,?,?)")
            .bind(movie).bind(root).bind(self.path.as_os_str().as_bytes()).bind(directory_identity(&self.directory)?).bind(package)
            .bind(directory_identity(&directory)?).bind(name).bind(attestation.0)
            .bind(stable_identity(&metadata)).bind(attestation.1)
            .bind(if attestation.2 { "worker" } else { "manual" }).execute(connection).await?;
        Ok(())
    }

    pub async fn bind_output_origin(
        connection: &mut sqlx::SqliteConnection,
        movie: &str,
        attempt: &str,
        conversion_publication: bool,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE movie_outputs SET origin='worker' WHERE movie_id=?
            AND (? OR EXISTS(SELECT 1 FROM conversion_jobs WHERE attempt_id=?))",
        )
        .bind(movie)
        .bind(conversion_publication)
        .bind(attempt)
        .execute(connection)
        .await?;
        Ok(())
    }

    pub async fn record_outputs(
        &self,
        connection: &mut sqlx::SqliteConnection,
        movie: &str,
        root: &str,
        package: &str,
    ) -> Result<()> {
        ensure!(
            uuid::Uuid::parse_str(package)?.to_string() == package,
            "invalid asset directory"
        );
        let directory = open_at(&self.directory, package, true)?;
        let directory_id = directory_identity(&directory)?;
        ensure!(matches!(root, "media" | "scratch"), "invalid asset root");
        let names = &["movie.mp4", "original-tracks.mkv"];
        for name in names {
            let file = match open_at(&directory, name, false) {
                Ok(file) => file,
                Err(error)
                    if error
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
                {
                    continue;
                }
                Err(error) => return Err(error),
            };
            let metadata = file.metadata()?;
            ensure!(
                metadata.is_file() && metadata.nlink() == 1,
                "generated asset must be an unshared regular file"
            );
            sqlx::query("INSERT INTO movie_outputs(movie_id,root,root_path,root_identity,directory,directory_identity,name,identity,stable_identity) VALUES(?,?,?,?,?,?,?,?,?)")
                .bind(movie).bind(root).bind(self.path.as_os_str().as_bytes()).bind(directory_identity(&self.directory)?).bind(package).bind(&directory_id).bind(name)
                .bind(crate::incoming::identity(&metadata)).bind(stable_identity(&metadata))
                .execute(&mut *connection).await?;
        }
        Ok(())
    }

    pub async fn verify_recorded_outputs(
        connection: &mut sqlx::SqliteConnection,
        movie: &str,
        root: &str,
        expected_movie: &str,
        expected_tracks: Option<&str>,
    ) -> Result<()> {
        let rows: Vec<(String, String)> =
            sqlx::query_as("SELECT name,identity FROM movie_outputs WHERE movie_id=? AND root=?")
                .bind(movie)
                .bind(root)
                .fetch_all(connection)
                .await?;
        ensure!(
            rows.len() == 1 + usize::from(expected_tracks.is_some()),
            "unexpected generated assets"
        );
        for (name, identity) in rows {
            let expected = match name.as_str() {
                "movie.mp4" => Some(expected_movie),
                "original-tracks.mkv" => expected_tracks,
                _ => None,
            };
            ensure!(
                expected == Some(identity.as_str()),
                "validated generated asset was replaced"
            );
        }
        Ok(())
    }

    pub async fn attest_output_hashes(
        connection: &mut sqlx::SqliteConnection,
        movie: &str,
        movie_hash: &str,
        tracks_hash: Option<&str>,
    ) -> Result<()> {
        sqlx::query("UPDATE movie_outputs SET sha256=CASE name WHEN 'movie.mp4' THEN ? ELSE ? END WHERE movie_id=?")
            .bind(movie_hash).bind(tracks_hash).bind(movie).execute(connection).await?;
        Ok(())
    }

    /// Recover a renamed file using its attested checksum outside the SQLite writer lock.
    pub fn verify_purge_intent(&self, output: &PurgeOutput<'_>) -> Result<Option<String>> {
        Self::validate_purge_output(output)?;
        ensure!(
            directory_identity(&self.directory)? == output.root_identity,
            "asset root was replaced"
        );
        let Some(directory) = optional_at(&self.directory, output.directory, true)? else {
            return Ok(None);
        };
        ensure!(
            directory_identity(&directory)? == output.directory_identity,
            "asset directory was replaced"
        );
        let target = match open_at(&directory, ".wetube-purge", true) {
            Ok(directory) => directory,
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
            {
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        let mut file = match open_at(&target, output.name, false) {
            Ok(file) => file,
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
            {
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        let metadata = file.metadata()?;
        ensure!(
            metadata.is_file()
                && metadata.nlink() == 1
                && stable_identity(&metadata) == output.stable_identity,
            "purge intent identity changed (recorded {}, current {})",
            output.stable_identity,
            stable_identity(&metadata)
        );
        let before = crate::incoming::identity(&metadata);
        let mut hash = Sha256::new();
        let mut bytes = [0; 128 * 1024];
        let mut count = 0u64;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3600);
        loop {
            ensure!(
                std::time::Instant::now() < deadline,
                "purge intent verification timed out"
            );
            let size = file.read(&mut bytes)?;
            if size == 0 {
                break;
            }
            count = count
                .checked_add(size as u64)
                .context("purge intent size overflow")?;
            ensure!(
                count <= metadata.len(),
                "purge intent grew during verification"
            );
            hash.update(&bytes[..size]);
        }
        ensure!(
            count == metadata.len() && hex::encode(hash.finalize()) == output.sha256,
            "purge intent checksum changed"
        );
        ensure!(
            crate::incoming::identity(&file.metadata()?) == before
                && crate::incoming::identity(&open_at(&target, output.name, false)?.metadata()?)
                    == before,
            "purge intent changed during verification"
        );
        Ok(Some(before))
    }

    fn validate_purge_output(output: &PurgeOutput<'_>) -> Result<()> {
        ensure!(
            uuid::Uuid::parse_str(output.directory)?.to_string() == output.directory,
            "invalid asset directory"
        );
        ensure!(
            matches!(output.name, "movie.mp4" | "original-tracks.mkv"),
            "not a purgeable generated asset"
        );
        Ok(())
    }

    /// Move into a private intent directory and validate the actual inode before unlinking.
    pub fn remove_output(
        &self,
        output: &PurgeOutput<'_>,
        verified_intent: Option<&str>,
    ) -> Result<()> {
        Self::validate_purge_output(output)?;
        let package = output.directory;
        let directory_id = output.directory_identity;
        let name = output.name;
        let expected = output.identity;
        let stable = output.stable_identity;
        ensure!(
            directory_identity(&self.directory)? == output.root_identity,
            "asset root was replaced"
        );
        let Some(directory) = optional_at(&self.directory, package, true)? else {
            return Ok(());
        };
        ensure!(
            directory_identity(&directory)? == directory_id,
            "asset directory was replaced"
        );
        let quarantine = CString::new(".wetube-purge")?;
        if unsafe { libc::mkdirat(directory.as_raw_fd(), quarantine.as_ptr(), 0o700) } != 0 {
            ensure!(
                std::io::Error::last_os_error().kind() == std::io::ErrorKind::AlreadyExists,
                "create purge intent directory"
            );
        }
        let target = open_at(&directory, ".wetube-purge", true)?;
        let pinned =
            |dir: &File| PathBuf::from(format!("/proc/self/fd/{}", dir.as_raw_fd())).join(name);
        let exists = |dir: &File| -> Result<bool> {
            match std::fs::symlink_metadata(pinned(dir)) {
                Ok(_) => Ok(true),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
                Err(e) => Err(e.into()),
            }
        };
        let mut staged_identity = None;
        if exists(&directory)? {
            ensure!(
                !exists(&target)?,
                "both source and purge intent exist; inspection required"
            );
            let file = open_at(&directory, name, false)?;
            let metadata = file.metadata()?;
            ensure!(
                metadata.is_file()
                    && metadata.nlink() == 1
                    && crate::incoming::identity(&metadata) == expected,
                "asset identity changed (stable fields match: {})",
                stable_identity(&metadata) == stable
            );
            let c_name = CString::new(name)?;
            ensure!(
                unsafe {
                    libc::renameat(
                        directory.as_raw_fd(),
                        c_name.as_ptr(),
                        target.as_raw_fd(),
                        c_name.as_ptr(),
                    )
                } == 0,
                "stage purge asset"
            );
            staged_identity = Some(crate::incoming::identity(&file.metadata().context(
                "read moved file identity; retained intent requires recovery verification",
            )?));
            target.sync_all()?;
            directory.sync_all()?;
        }
        if exists(&target)? {
            let file = open_at(&target, name, false)?;
            let metadata = file.metadata()?;
            ensure!(
                metadata.is_file() && metadata.nlink() == 1 && stable_identity(&metadata) == stable,
                "purge intent identity changed"
            );
            let before = crate::incoming::identity(&metadata);
            // Recovery hashes outside SQLite's writer lock, then deletion checks
            // the complete identity (including ctime) again under that lock.
            ensure!(
                staged_identity.as_deref().or(verified_intent) == Some(before.as_str()),
                "staged asset changed or recovery verification is required"
            );
            // The intent directory is private to the maintenance-lock owner.
            ensure!(
                crate::incoming::identity(&file.metadata()?) == before
                    && crate::incoming::identity(&open_at(&target, name, false)?.metadata()?)
                        == before,
                "purge intent replaced"
            );
            let name = CString::new(name)?;
            ensure!(
                unsafe { libc::unlinkat(target.as_raw_fd(), name.as_ptr(), 0) } == 0,
                "unlink verified purge asset"
            );
            target.sync_all()?;
        }
        Ok(())
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
