//! `FileSystem` — reading and writing wherever the tools' files live.

use std::path::{Path, PathBuf};

use super::ExecError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Metadata {
    pub is_dir: bool,
    pub is_file: bool,
    pub is_symlink: bool,
    pub len: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    pub path: PathBuf,
    pub is_dir: bool,
}

/// Where a tool's files are.
///
/// Whole-value rather than streaming, which is the opposite of [`Process`] and
/// deliberately so: every call site in the engine reads or writes a whole
/// file, and a tool result is capped long before a file could be large enough
/// to need chunking. Designing a streaming interface with no consumer would
/// oblige every implementation to provide one.
///
/// **Path safety is not here.** Whether a path may be written is a policy —
/// it answers the same question regardless of which machine the file is on,
/// and a provider that decided its own out-of-bounds rules could cancel them.
/// It stays in `permissions::path_safety`, above this contract.
///
/// One thing does cross: [`canonicalize`](Self::canonicalize). A remote's
/// symlink graph is the remote's, so resolving one here would answer about the
/// wrong filesystem. The order is: canonicalize through the provider, check
/// the resolved path against the policy, then write through the provider.
///
/// [`Process`]: super::Process
#[async_trait::async_trait]
pub trait FileSystem: Send + Sync {
    async fn read(&self, path: &Path) -> Result<Vec<u8>, ExecError>;
    async fn write(&self, path: &Path, bytes: &[u8]) -> Result<(), ExecError>;
    async fn create_dir_all(&self, path: &Path) -> Result<(), ExecError>;
    async fn read_dir(&self, path: &Path) -> Result<Vec<DirEntry>, ExecError>;
    async fn remove_dir_all(&self, path: &Path) -> Result<(), ExecError>;
    async fn metadata(&self, path: &Path) -> Result<Metadata, ExecError>;
    async fn canonicalize(&self, path: &Path) -> Result<PathBuf, ExecError>;

    /// Move `from` to `to` on this filesystem. Provided, for a provider
    /// that cannot: it fails and moves nothing.
    async fn rename(&self, from: &Path, _to: &Path) -> Result<(), ExecError> {
        Err(ExecError::failed(format!(
            "{}: this filesystem does not rename",
            from.display()
        )))
    }

    /// Remove the file at `path` — a symlink itself, never what it points
    /// at. Provided, for a provider that cannot: it fails and removes nothing.
    async fn remove_file(&self, path: &Path) -> Result<(), ExecError> {
        Err(ExecError::failed(format!(
            "{}: this filesystem does not remove files",
            path.display()
        )))
    }

    /// [`write`](Self::write), refusing when `path` itself is a symlink
    /// rather than writing where it points. Provided as a check then a write;
    /// a provider that can do both in one step (`O_NOFOLLOW`) should.
    async fn write_not_following(&self, path: &Path, bytes: &[u8]) -> Result<(), ExecError> {
        if self.metadata(path).await.is_ok_and(|m| m.is_symlink) {
            return Err(ExecError::failed(format!(
                "{} is a symlink",
                path.display()
            )));
        }
        self.write(path, bytes).await
    }

    /// The file's sha256 as lowercase hex, hashed without holding the file.
    /// `None` for a provider that cannot, which the caller answers by
    /// reading — and so only for files it can afford to read.
    async fn sha256(&self, _path: &Path) -> Result<Option<String>, ExecError> {
        Ok(None)
    }

    /// What most callers actually want. Provided, so an implementor writes
    /// `read` and gets this.
    async fn read_to_string(&self, path: &Path) -> Result<String, ExecError> {
        let bytes = self.read(path).await?;
        String::from_utf8(bytes)
            .map_err(|e| ExecError::failed(format!("{} is not valid UTF-8: {e}", path.display())))
    }

    /// Convenience with the same relationship to `write`.
    async fn write_str(&self, path: &Path, text: &str) -> Result<(), ExecError> {
        self.write(path, text.as_bytes()).await
    }

    /// Metadata of what `path` points at rather than of `path` itself.
    ///
    /// [`metadata`](Self::metadata) reports on the link — that is what its
    /// `is_symlink` field is for — so a caller asking "how big is this, is it
    /// a directory" has to resolve first or a symlink to a directory answers
    /// as a small ordinary file.
    async fn metadata_following_symlinks(&self, path: &Path) -> Result<Metadata, ExecError> {
        let resolved = self.canonicalize(path).await?;
        self.metadata(&resolved).await
    }

    /// What the symbolic link at `path` points at, as written in the link.
    /// Provided, for a provider with no links: it fails.
    async fn read_link(&self, path: &Path) -> Result<PathBuf, ExecError> {
        Err(ExecError::failed(format!(
            "{}: this filesystem has no symbolic links",
            path.display()
        )))
    }

    /// The resolved form of a path that may not exist yet: its deepest prefix
    /// that exists, resolved by [`canonicalize`](Self::canonicalize), with
    /// the rest put back on.
    ///
    /// A write policy has to be checked against where the write lands. The
    /// rest does not exist, so it has no symlinks — except a link whose
    /// target does not exist yet, which `canonicalize` cannot resolve and a
    /// write follows; that one is followed here with
    /// [`read_link`](Self::read_link).
    async fn canonicalize_best_effort(&self, path: &Path) -> PathBuf {
        use crate::path::{first_below, put_back, MAX_LINK_HOPS};
        let mut wanted = path.to_path_buf();
        for _ in 0..=MAX_LINK_HOPS {
            let parts: Vec<std::path::Component> = wanted.components().collect();
            let mut found = None;
            for split in (1..=parts.len()).rev() {
                let prefix: PathBuf = parts[..split].iter().collect();
                if let Ok(out) = self.canonicalize(&prefix).await {
                    found = Some((split, out));
                    break;
                }
            }
            let Some((split, out)) = found else {
                return wanted;
            };
            let rest = &parts[split..];
            let target = match first_below(&out, rest) {
                Some((link, after)) => self.read_link(&link).await.ok().map(|t| (t, after)),
                None => None,
            };
            match target {
                Some((target, after)) => wanted = out.join(target).join(after),
                None => return put_back(out, rest),
            }
        }
        wanted
    }
}
