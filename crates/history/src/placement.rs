//! 会话历史放在哪：全局状态根，还是会话所属项目的 `state/history/<id>/`。
//!
//! 宿主经 [`ProjectHistory`] 回答三件事：一个项目把历史放在哪个目录（worktree 归主仓库）、
//! 现在新建的会话进不进项目、项目此刻是否受信。放进项目的会话在全局 `sessions/<id>/`
//! 留一份 [`SessionIndex`]，存储只读索引登记过的项目内会话；项目目录里多出来的一律不认。

use crate::entry::SessionKind;
use crate::error::HistoryError;
use crate::path::session_index_file;
use crate::store::SessionFacts;
use base::session::SessionId;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// What a store asks its host about projects.
pub trait ProjectHistory: Send + Sync {
    /// Where the project at `root` keeps history — `None` when it keeps none.
    /// For a worktree it is the main checkout's.
    fn history_of(&self, root: &Path) -> Option<ProjectHistoryDir>;

    /// Whether a session created now under `root` is kept in its project.
    fn keeps_new(&self, root: &Path) -> bool;

    /// Whether history kept in the project at `root` may be read now.
    fn trusted(&self, root: &Path) -> bool;
}

/// A project's history directory and the project it belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectHistoryDir {
    pub root: PathBuf,
    pub dir: PathBuf,
}

pub const INDEX_SCHEMA_VERSION: u32 = 1;

/// The global record of a session kept in a project: `sessions/<id>/index.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionIndex {
    pub schema_version: u32,
    pub session_id: SessionId,
    /// The project the history is kept in; for a worktree's session, the
    /// main checkout.
    pub project_root: PathBuf,
    /// `<project_root>/<project dir>/state/history/<id>`.
    pub dir: PathBuf,
    /// The project root as the session's log spells it, where that is not
    /// `project_root`: another spelling of the same directory, or where the
    /// project was before it moved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recorded_root: Option<PathBuf>,
    /// The lineage the session's `Meta` recorded, kept here too so that a
    /// cascade delete finds a project's sidechains while the project is not
    /// trusted and its logs are not read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<SessionId>,
    #[serde(default)]
    pub session_kind: SessionKind,
}

impl SessionIndex {
    pub fn new(session: SessionId, project_root: PathBuf, history_dir: &Path) -> Self {
        Self {
            schema_version: INDEX_SCHEMA_VERSION,
            session_id: session,
            project_root,
            dir: history_dir.join(session.to_string()),
            recorded_root: None,
            parent_session_id: None,
            session_kind: SessionKind::Primary,
        }
    }

    /// What the index knows of the session's `Meta`: its lineage.
    pub fn facts(&self) -> SessionFacts {
        SessionFacts {
            scene: None,
            project_root: None,
            session_kind: self.session_kind,
            parent_session_id: self.parent_session_id.map(|p| p.to_string()),
        }
    }

    /// The directory the session's directory sits in: the project's
    /// `state/history`.
    pub fn history_dir(&self) -> &Path {
        self.dir.parent().unwrap_or(&self.dir)
    }

    /// The project root as the session's own log records it.
    pub fn recorded_root(&self) -> &Path {
        self.recorded_root.as_deref().unwrap_or(&self.project_root)
    }

    /// `session`'s index under `sessions_root`; `None` when it has none,
    /// which is a session kept globally.
    pub fn read(sessions_root: &Path, session: SessionId) -> Result<Option<Self>, HistoryError> {
        let path = session_index_file(sessions_root, &session);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(HistoryError::Io(e)),
        };
        let index: Self = serde_json::from_str(&text)?;
        let names_itself =
            index.dir.file_name().and_then(|n| n.to_str()) == Some(session.to_string().as_str());
        if index.session_id != session
            || !names_itself
            || !index.dir.is_absolute()
            || !index.project_root.is_absolute()
        {
            return Err(HistoryError::Path(format!(
                "{} does not describe session {session}",
                path.display()
            )));
        }
        Ok(Some(index))
    }

    pub fn write(&self, sessions_root: &Path) -> Result<(), HistoryError> {
        let path = session_index_file(sessions_root, &self.session_id);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        base::fs::write_atomic(
            &path,
            &serde_json::to_vec_pretty(self)?,
            base::fs::WriteOpts::DURABLE,
        )?;
        Ok(())
    }
}

/// One session's sidecar directory, found again on every call: it depends on
/// the index, which the session's `Meta` writes, and on the project's trust.
#[derive(Clone)]
pub struct SidecarLocator(Arc<dyn Fn() -> Result<PathBuf, HistoryError> + Send + Sync>);

impl SidecarLocator {
    pub fn new(locate: impl Fn() -> Result<PathBuf, HistoryError> + Send + Sync + 'static) -> Self {
        Self(Arc::new(locate))
    }

    pub fn fixed(dir: PathBuf) -> Self {
        Self::new(move || Ok(dir.clone()))
    }

    pub fn dir(&self) -> Result<PathBuf, HistoryError> {
        (self.0)()
    }

    pub fn file(&self, name: &str) -> Result<PathBuf, HistoryError> {
        Ok(self.dir()?.join(name))
    }
}

impl std::fmt::Debug for SidecarLocator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SidecarLocator")
    }
}
