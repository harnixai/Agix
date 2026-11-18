//! `LocalFileSystem` — this machine's disk.

use std::path::{Path, PathBuf};

use base::interface::exec::{DirEntry, ExecError, FileSystem, Metadata};

pub struct LocalFileSystem;

/// Classify an io error the way the contract's three sentences require: a
/// permission refusal is the operating system's policy talking, everything
/// else is the target failing. Nothing here is ever `Unavailable` — the local
/// disk is the one provider that cannot be out of reach.
fn failed(path: &Path, e: std::io::Error) -> ExecError {
    if e.kind() == std::io::ErrorKind::PermissionDenied {
        ExecError::denied(format!("{}: {e}", path.display()))
    } else {
        ExecError::failed(format!("{}: {e}", path.display()))
    }
}

#[async_trait::async_trait]
impl FileSystem for LocalFileSystem {
    async fn read(&self, path: &Path) -> Result<Vec<u8>, ExecError> {
        tokio::fs::read(path).await.map_err(|e| failed(path, e))
    }

    async fn sha256(&self, path: &Path) -> Result<Option<String>, ExecError> {
        use sha2::Digest;
        use std::io::Read;
        let owned = path.to_path_buf();
        tokio::task::spawn_blocking(move || {
            let mut file = std::fs::File::open(&owned)?;
            let mut hasher = sha2::Sha256::new();
            let mut chunk = vec![0u8; 64 * 1024];
            loop {
                match file.read(&mut chunk)? {
                    0 => return Ok(Some(hex::encode(hasher.finalize()))),
                    n => hasher.update(&chunk[..n]),
                }
            }
        })
        .await
        .map_err(|e| ExecError::failed(format!("{}: {e}", path.display())))?
        .map_err(|e| failed(path, e))
    }

    async fn write(&self, path: &Path, bytes: &[u8]) -> Result<(), ExecError> {
        tokio::fs::write(path, bytes)
            .await
            .map_err(|e| failed(path, e))
    }

    async fn write_not_following(&self, path: &Path, bytes: &[u8]) -> Result<(), ExecError> {
        let (owned, bytes) = (path.to_path_buf(), bytes.to_vec());
        let written = tokio::task::spawn_blocking(move || {
            use std::io::Write;
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create(true).truncate(true);
            #[cfg(unix)]
            std::os::unix::fs::OpenOptionsExt::custom_flags(&mut options, libc::O_NOFOLLOW);
            options.open(&owned)?.write_all(&bytes)
        })
        .await
        .map_err(|e| ExecError::failed(format!("{}: {e}", path.display())))?;
        written.map_err(|e| failed(path, e))
    }

    #[cfg(unix)]
    async fn write_resolved(
        &self,
        path: &Path,
        bytes: &[u8],
        create_parents: bool,
    ) -> Result<(), ExecError> {
        let (owned, bytes) = (path.to_path_buf(), bytes.to_vec());
        tokio::task::spawn_blocking(move || {
            unlinked::write(&owned, &bytes, create_parents).map_err(|e| {
                if e.raw_os_error() == Some(libc::ELOOP) {
                    ExecError::failed(format!(
                        "{}: a symlink took the place of a directory or of the file \
                         after the path was checked",
                        owned.display()
                    ))
                } else {
                    failed(&owned, e)
                }
            })
        })
        .await
        .map_err(|e| ExecError::failed(format!("{}: {e}", path.display())))?
    }

    async fn create_dir_all(&self, path: &Path) -> Result<(), ExecError> {
        tokio::fs::create_dir_all(path)
            .await
            .map_err(|e| failed(path, e))
    }

    async fn read_dir(&self, path: &Path) -> Result<Vec<DirEntry>, ExecError> {
        let mut rd = tokio::fs::read_dir(path)
            .await
            .map_err(|e| failed(path, e))?;
        let mut out = Vec::new();
        while let Some(e) = rd.next_entry().await.map_err(|e| failed(path, e))? {
            let is_dir = e
                .file_type()
                .await
                .map(|t| t.is_dir())
                .map_err(|e| failed(path, e))?;
            out.push(DirEntry {
                path: e.path(),
                is_dir,
            });
        }
        Ok(out)
    }

    async fn remove_dir_all(&self, path: &Path) -> Result<(), ExecError> {
        tokio::fs::remove_dir_all(path)
            .await
            .map_err(|e| failed(path, e))
    }

    async fn metadata(&self, path: &Path) -> Result<Metadata, ExecError> {
        // `symlink_metadata` rather than `metadata`, because the contract has
        // an `is_symlink` field and `metadata` follows links — it can never
        // report one.
        let m = tokio::fs::symlink_metadata(path)
            .await
            .map_err(|e| failed(path, e))?;
        Ok(Metadata {
            is_dir: m.is_dir(),
            is_file: m.is_file(),
            is_symlink: m.file_type().is_symlink(),
            len: m.len(),
        })
    }

    async fn remove_file(&self, path: &Path) -> Result<(), ExecError> {
        tokio::fs::remove_file(path)
            .await
            .map_err(|e| failed(path, e))
    }

    async fn rename(&self, from: &Path, to: &Path) -> Result<(), ExecError> {
        tokio::fs::rename(from, to)
            .await
            .map_err(|e| failed(from, e))
    }

    async fn canonicalize(&self, path: &Path) -> Result<PathBuf, ExecError> {
        tokio::fs::canonicalize(path)
            .await
            .map_err(|e| failed(path, e))
    }

    async fn read_link(&self, path: &Path) -> Result<PathBuf, ExecError> {
        tokio::fs::read_link(path)
            .await
            .map_err(|e| failed(path, e))
    }
}

/// Opening a path one directory at a time, from `/`, following no symlink.
#[cfg(unix)]
mod unlinked {
    use std::ffi::{CString, OsStr};
    use std::io::{Error, ErrorKind, Write};
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    use std::path::{Component, Path};

    #[cfg(any(target_os = "linux", target_os = "android"))]
    const DIR_FLAGS: libc::c_int = libc::O_PATH | libc::O_DIRECTORY;
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    const DIR_FLAGS: libc::c_int = libc::O_RDONLY | libc::O_DIRECTORY;

    pub(super) fn write(path: &Path, bytes: &[u8], create_parents: bool) -> std::io::Result<()> {
        let mut components = path.components();
        if components.next() != Some(Component::RootDir) {
            return Err(not_resolved(path));
        }
        let names = components
            .map(|c| match c {
                Component::Normal(name) => c_name(name),
                _ => Err(not_resolved(path)),
            })
            .collect::<std::io::Result<Vec<_>>>()?;
        let Some((file, dirs)) = names.split_last() else {
            return Err(not_resolved(path));
        };
        let mut dir = open_at(libc::AT_FDCWD, c"/", DIR_FLAGS, 0)?;
        for name in dirs {
            let opened = open_at(dir.as_raw_fd(), name, DIR_FLAGS | libc::O_NOFOLLOW, 0)
                .map_err(|e| as_link_refusal(&dir, name, e));
            dir = match opened {
                Err(e) if e.kind() == ErrorKind::NotFound && create_parents => {
                    // SAFETY: `dir` is an open directory and `name` a NUL-terminated name.
                    if unsafe { libc::mkdirat(dir.as_raw_fd(), name.as_ptr(), 0o777) } != 0 {
                        let e = Error::last_os_error();
                        if e.kind() != ErrorKind::AlreadyExists {
                            return Err(e);
                        }
                    }
                    open_at(dir.as_raw_fd(), name, DIR_FLAGS | libc::O_NOFOLLOW, 0)?
                }
                opened => opened?,
            };
        }
        let flags = libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC | libc::O_NOFOLLOW;
        let file = open_at(dir.as_raw_fd(), file, flags, 0o666)?;
        std::fs::File::from(file).write_all(bytes)
    }

    fn open_at(
        dir: libc::c_int,
        name: &std::ffi::CStr,
        flags: libc::c_int,
        mode: libc::c_uint,
    ) -> std::io::Result<OwnedFd> {
        // SAFETY: `name` is NUL-terminated; a non-negative result is a
        // descriptor nothing else owns.
        let fd = unsafe { libc::openat(dir, name.as_ptr(), flags | libc::O_CLOEXEC, mode) };
        if fd < 0 {
            return Err(Error::last_os_error());
        }
        // SAFETY: as above.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    /// macOS answers a directory open that meets a symlink with `ENOTDIR`;
    /// told apart from a file on the way, it is `ELOOP` as on Linux.
    fn as_link_refusal(dir: &OwnedFd, name: &std::ffi::CStr, e: Error) -> Error {
        if e.raw_os_error() != Some(libc::ENOTDIR) {
            return e;
        }
        // SAFETY: `stat` is plain old data the call fills in.
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: `dir` is open, `name` NUL-terminated, `stat` writable.
        let found = unsafe {
            libc::fstatat(
                dir.as_raw_fd(),
                name.as_ptr(),
                &mut stat,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if found == 0 && stat.st_mode & libc::S_IFMT == libc::S_IFLNK {
            Error::from_raw_os_error(libc::ELOOP)
        } else {
            e
        }
    }

    fn c_name(name: &OsStr) -> std::io::Result<CString> {
        CString::new(name.as_bytes()).map_err(|e| Error::new(ErrorKind::InvalidInput, e))
    }

    fn not_resolved(path: &Path) -> Error {
        Error::new(
            ErrorKind::InvalidInput,
            format!(
                "{} is not an absolute path with its links resolved",
                path.display()
            ),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_round_trip_through_the_contract_is_a_round_trip_through_the_disk() {
        let dir = tempfile::tempdir().unwrap();
        let fs = LocalFileSystem;
        let f = dir.path().join("a/b/note.txt");
        fs.create_dir_all(f.parent().unwrap()).await.unwrap();
        fs.write_str(&f, "hello").await.unwrap();
        assert_eq!(fs.read_to_string(&f).await.unwrap(), "hello");
        assert!(fs.metadata(&f).await.unwrap().is_file);
        assert_eq!(fs.read_dir(f.parent().unwrap()).await.unwrap().len(), 1);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn removing_a_symlink_removes_the_link_and_not_its_target() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target.txt");
        let link = dir.path().join("link.txt");
        std::fs::write(&target, "kept").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        LocalFileSystem.remove_file(&link).await.unwrap();
        assert!(std::fs::symlink_metadata(&link).is_err());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "kept");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_resolved_write_follows_no_symlink_on_the_way() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let (real, outside) = (root.join("real"), root.join("outside"));
        std::fs::create_dir_all(&real).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("dir-link")).unwrap();
        std::os::unix::fs::symlink(outside.join("f"), real.join("file-link")).unwrap();
        let fs = LocalFileSystem;

        fs.write_resolved(&real.join("a/b/new.txt"), b"x", true)
            .await
            .unwrap();
        assert_eq!(std::fs::read(real.join("a/b/new.txt")).unwrap(), b"x");
        assert!(fs
            .write_resolved(&real.join("c/new.txt"), b"x", false)
            .await
            .is_err());
        assert!(!real.join("c").exists());

        for (through, created) in [
            (root.join("dir-link/x"), outside.join("x")),
            (root.join("dir-link/sub/x"), outside.join("sub")),
            (real.join("file-link"), outside.join("f")),
        ] {
            let e = fs.write_resolved(&through, b"x", true).await.unwrap_err();
            assert!(e.to_string().contains("symlink"), "{e}");
            assert!(!created.exists(), "{}", through.display());
        }
    }

    #[tokio::test]
    async fn a_missing_file_is_the_target_failing_not_the_provider() {
        let dir = tempfile::tempdir().unwrap();
        let e = LocalFileSystem
            .read(&dir.path().join("nope"))
            .await
            .unwrap_err();
        assert!(
            matches!(e, ExecError::Failed(_)),
            "a host that saw `Unavailable` here would go looking for a broken \
             execution environment: {e:?}"
        );
    }

    /// The reason `metadata` does not use `tokio::fs::metadata`.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_symlink_reports_as_one() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        let link = dir.path().join("link");
        LocalFileSystem.write_str(&target, "x").await.unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(LocalFileSystem.metadata(&link).await.unwrap().is_symlink);
        assert_eq!(
            LocalFileSystem.canonicalize(&link).await.unwrap(),
            tokio::fs::canonicalize(&target).await.unwrap(),
            "and canonicalize resolves it, which is why path safety has to run \
             on the result rather than the argument"
        );
    }
}
