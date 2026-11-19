//! 文件级 checkpoint：文件工具每次写之前、之后的样子（`agix.checkpoint` 条目），宿主的回滚
//! （`agix.rewind` 条目），以及写前副本的存放处——按完整 sha256 存在根会话 sidecar 的
//! `checkpoints/objects/` 下。条目都写在根会话的日志里，子 Agent 的写也是；回滚该恢复什么、
//! 该作废哪段对话由 [`plan`] 从这些条目算出来。

pub mod plan;

use crate::blob::BlobStore;
use base::id::Id;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub const CHECKPOINT_NS: &str = "agix.checkpoint";
pub const PRE: &str = "pre";
pub const POST: &str = "post";
pub const PRUNED: &str = "pruned";

pub const REWIND_NS: &str = "agix.rewind";
pub const BEGIN: &str = "begin";
pub const COMMIT: &str = "commit";
pub const ABORT: &str = "abort";

/// Above this a file's previous content is hashed but not kept.
pub const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;

/// A written file as the log names it: relative to the project root when it
/// is inside it, so a log imported under another root still points into it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordedPath {
    Rel(String),
    Abs(String),
}

impl RecordedPath {
    /// `None` for a path that is not UTF-8.
    pub fn of(target: &Path, project_root: Option<&Path>) -> Option<Self> {
        if let Some(rest) = project_root.and_then(|root| target.strip_prefix(root).ok()) {
            if !rest.as_os_str().is_empty() {
                return rest.to_str().map(|s| Self::Rel(s.to_string()));
            }
        }
        target.to_str().map(|s| Self::Abs(s.to_string()))
    }

    /// Where it is on this machine; `None` for a relative record without a
    /// project to put it in.
    pub fn resolve(&self, project_root: Option<&Path>) -> Option<PathBuf> {
        match self {
            Self::Rel(rel) => project_root.map(|root| root.join(rel)),
            Self::Abs(abs) => Some(PathBuf::from(abs)),
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::Rel(s) | Self::Abs(s) => s,
        }
    }

    pub fn outside_project(&self) -> bool {
        matches!(self, Self::Abs(_))
    }
}

/// What a file was before a write.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Before {
    File {
        sha256: String,
        bytes: u64,
    },
    Absent,
    /// Over [`MAX_FILE_BYTES`]: hashed, not kept.
    TooLarge {
        sha256: String,
        bytes: u64,
    },
    Unreadable {
        error: String,
    },
    NotAFile,
}

/// What a file was after a write, or after the engine last looked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum After {
    File { sha256: String },
    Absent,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreFile {
    pub path: RecordedPath,
    pub before: Before,
}

/// A write made by a delegate of the root session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DelegateAttribution {
    pub session_id: String,
    pub turn_id: Option<String>,
    pub parent_tool_use_id: Option<String>,
    pub depth: u32,
}

/// `agix.checkpoint/pre`, written and synced before the write it describes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreRecord {
    /// What the matching `post` names it by.
    pub id: String,
    pub files: Vec<PreFile>,
    pub tool: String,
    pub tool_use_id: Option<String>,
    #[serde(default)]
    pub rewind_id: Option<String>,
    #[serde(default)]
    pub agent: Option<DelegateAttribution>,
}

/// `agix.checkpoint/pruned`: the copies named by `pre` lines up to and
/// including `before` may have been deleted to stay under the quota.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrunedRecord {
    pub before: String,
}

/// `agix.checkpoint/post`, after the write. `after` follows `files` of its
/// `pre` one for one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PostRecord {
    pub pre: String,
    pub ok: bool,
    pub after: Vec<After>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RewindTarget {
    ToTurn(String),
    ToToolUse(String),
    ToStart(bool),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RewindScope {
    Files,
    Conversation,
    Both,
}

impl RewindScope {
    pub fn files(self) -> bool {
        matches!(self, Self::Files | Self::Both)
    }

    pub fn conversation(self) -> bool {
        matches!(self, Self::Conversation | Self::Both)
    }
}

/// `agix.rewind/begin`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RewindBegin {
    pub rewind_id: String,
    pub target: RewindTarget,
    pub scope: RewindScope,
    pub cut_after: Option<Id>,
    pub paths: Vec<String>,
}

/// `agix.rewind/commit`. With `conversation`, every voidable line after
/// `cut_after` and before this one is out of the conversation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RewindCommit {
    pub rewind_id: String,
    pub cut_after: Option<Id>,
    pub conversation: bool,
    pub files_restored: usize,
}

/// `agix.rewind/abort`: the files this rewind changed were put back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RewindAbort {
    pub rewind_id: String,
    pub failed: String,
    pub rolled_back: Vec<String>,
    pub not_rolled_back: Vec<String>,
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// A file's previous contents, by their full sha256 — a collision here is a
/// file written back with someone else's contents, which a 16-character id
/// would not rule out.
///
/// What comes back has been hashed again: an object whose bytes no longer
/// match its name is reported missing, so tampering with the directory can
/// make a rewind do less, never write something else.
#[derive(Debug, Clone)]
pub struct CheckpointObjects {
    dir: ObjectsDir,
}

#[derive(Debug, Clone)]
enum ObjectsDir {
    Fixed(PathBuf),
    /// `checkpoints/objects` in a session's sidecar, wherever that is when asked.
    Sidecar(crate::placement::SidecarLocator),
}

impl CheckpointObjects {
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir: ObjectsDir::Fixed(dir),
        }
    }

    pub fn located(sidecar: crate::placement::SidecarLocator) -> Self {
        Self {
            dir: ObjectsDir::Sidecar(sidecar),
        }
    }

    fn dir(&self) -> Result<PathBuf, std::io::Error> {
        match &self.dir {
            ObjectsDir::Fixed(dir) => Ok(dir.clone()),
            ObjectsDir::Sidecar(sidecar) => sidecar
                .file(crate::path::CHECKPOINTS_DIR)
                .map(|dir| dir.join("objects"))
                .map_err(|e| match e {
                    crate::error::HistoryError::Io(e) => e,
                    other => {
                        std::io::Error::new(std::io::ErrorKind::PermissionDenied, other.to_string())
                    }
                }),
        }
    }
}

fn is_object_id(id: &str) -> bool {
    id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit())
}

impl BlobStore for CheckpointObjects {
    fn name(&self) -> &str {
        "checkpoint"
    }

    fn put(&self, bytes: &[u8]) -> Result<String, std::io::Error> {
        let id = sha256_hex(bytes);
        let dir = self.dir()?;
        create_private_dir(&dir)?;
        let path = dir.join(&id);
        let whole = std::fs::metadata(&path).is_ok_and(|m| m.len() == bytes.len() as u64);
        if !whole {
            base::fs::write_atomic(&path, bytes, base::fs::WriteOpts::PRIVATE)?;
        }
        Ok(id)
    }

    fn get(&self, id: &str) -> Result<Option<Vec<u8>>, std::io::Error> {
        if !is_object_id(id) {
            return Ok(None);
        }
        match std::fs::read(self.dir()?.join(id)) {
            Ok(bytes) if sha256_hex(&bytes) == id => Ok(Some(bytes)),
            Ok(_) => Ok(None),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn list(&self) -> Result<Vec<(String, u64)>, std::io::Error> {
        let entries = match std::fs::read_dir(self.dir()?) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        let mut objects = Vec::new();
        for entry in entries {
            let entry = entry?;
            if let Some(id) = entry.file_name().to_str().filter(|id| is_object_id(id)) {
                objects.push((id.to_string(), entry.metadata()?.len()));
            }
        }
        Ok(objects)
    }

    fn remove(&self, id: &str) -> Result<(), std::io::Error> {
        if !is_object_id(id) {
            return Ok(());
        }
        match std::fs::remove_file(self.dir()?.join(id)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }
}

fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    if dir.is_dir() {
        return Ok(());
    }
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for d in [Some(dir), dir.parent()].into_iter().flatten() {
            std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o700))?;
        }
    }
    Ok(())
}

/// The same without a disk, for a store that keeps its logs in memory.
#[derive(Debug, Default)]
pub struct InMemoryCheckpointObjects {
    objects: std::sync::Mutex<HashMap<String, Vec<u8>>>,
}

impl BlobStore for InMemoryCheckpointObjects {
    fn name(&self) -> &str {
        "checkpoint"
    }

    fn put(&self, bytes: &[u8]) -> Result<String, std::io::Error> {
        let id = sha256_hex(bytes);
        self.objects
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(id.clone())
            .or_insert_with(|| bytes.to_vec());
        Ok(id)
    }

    fn get(&self, id: &str) -> Result<Option<Vec<u8>>, std::io::Error> {
        Ok(self
            .objects
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .filter(|bytes| sha256_hex(bytes) == id)
            .cloned())
    }

    fn list(&self) -> Result<Vec<(String, u64)>, std::io::Error> {
        Ok(self
            .objects
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(id, bytes)| (id.clone(), bytes.len() as u64))
            .collect())
    }

    fn remove(&self, id: &str) -> Result<(), std::io::Error> {
        self.objects
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(id);
        Ok(())
    }
}

#[cfg(test)]
mod tests;
