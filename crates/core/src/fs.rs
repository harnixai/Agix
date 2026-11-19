//! Atomic file replacement shared by every crate that persists state, and
//! opening a file that must be the path itself rather than what a symlink
//! there points at.
//!
//! `write_atomic` is the one implementation of "write a temp file next to the
//! target, then rename it into place". Callers keep their own directory
//! creation and any policy about symlinks; this module only guarantees that a
//! reader sees the old file or the new one, never part of one.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Debug, Clone, Copy)]
pub struct WriteOpts {
    pub mode: Option<u32>,
    pub fsync: bool,
}

impl WriteOpts {
    pub const DURABLE: Self = Self {
        mode: None,
        fsync: true,
    };
    pub const PRIVATE: Self = Self {
        mode: Some(0o600),
        fsync: true,
    };
    pub const FAST: Self = Self {
        mode: None,
        fsync: false,
    };
}

fn temp_sibling(path: &Path) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    path.with_file_name(format!(
        ".{name}.{}-{}-{nanos:08x}.tmp",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed),
    ))
}

/// `bytes` to `path` so a reader sees the old file or the new one, never part of one.
pub fn write_atomic(path: &Path, bytes: &[u8], opts: WriteOpts) -> std::io::Result<()> {
    let tmp = temp_sibling(path);
    let result = write_temp(&tmp, bytes, opts).and_then(|()| std::fs::rename(&tmp, path));
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
        return result;
    }
    #[cfg(unix)]
    if opts.fsync {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            if let Ok(d) = std::fs::File::open(dir) {
                let _ = d.sync_all();
            }
        }
    }
    Ok(())
}

/// Options that open the path itself: on Unix a symlink there fails the open
/// (`ELOOP`) instead of being followed.
pub fn no_follow() -> std::fs::OpenOptions {
    #[allow(unused_mut)]
    let mut options = std::fs::OpenOptions::new();
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::custom_flags(&mut options, libc::O_NOFOLLOW);
    options
}

/// `path`'s bytes, refusing a symlink at `path`.
pub fn read_no_follow(path: &Path) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let mut bytes = Vec::new();
    no_follow().read(true).open(path)?.read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// [`read_no_follow`] as text.
pub fn read_to_string_no_follow(path: &Path) -> std::io::Result<String> {
    String::from_utf8(read_no_follow(path)?)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

fn write_temp(tmp: &Path, bytes: &[u8], opts: WriteOpts) -> std::io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    if let Some(mode) = opts.mode {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(mode);
    }
    let mut file = options.open(tmp)?;
    #[cfg(unix)]
    if let Some(mode) = opts.mode {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(mode))?;
    }
    file.write_all(bytes)?;
    if opts.fsync {
        file.sync_all()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_the_file_and_leaves_no_temp_behind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.json");
        write_atomic(&path, b"one", WriteOpts::DURABLE).unwrap();
        write_atomic(&path, b"two", WriteOpts::FAST).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"two");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn a_missing_parent_is_an_error_not_created() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing").join("a");
        assert!(write_atomic(&path, b"x", WriteOpts::DURABLE).is_err());
        assert!(!dir.path().join("missing").exists());
    }

    #[cfg(unix)]
    #[test]
    fn private_mode_survives_a_permissive_umask_and_a_rewrite() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret");
        write_atomic(&path, b"1", WriteOpts::PRIVATE).unwrap();
        write_atomic(&path, b"2", WriteOpts::PRIVATE).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_is_not_read_through() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        std::fs::write(&target, "secret").unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(read_no_follow(&link).is_err());
        assert_eq!(read_to_string_no_follow(&target).unwrap(), "secret");
    }

    #[test]
    fn temp_names_differ_within_one_process() {
        let p = Path::new("/x/y.json");
        assert_ne!(temp_sibling(p), temp_sibling(p));
    }
}
