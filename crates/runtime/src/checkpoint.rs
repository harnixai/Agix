//! 文件工具写盘前后的快照：工具管线在一次调用真正写盘之前记下目标文件此刻的内容（`pre`，
//! 落盘并 sync 之后才放行），写完之后记下它变成了什么（`post`）。条目写进根会话的日志，
//! 子 Agent 的写也是——句柄只换署名不换根。回滚自己的写也走这里，所以回滚可以被下一次回滚撤销。
//!
//! 认"这次调用写哪个文件"凭的是工具声明的 `Subject::WritePath`，和 `Edit(...)` 权限规则认写
//! 用的是同一份声明；不声明的（Bash、MCP 工具）不在这里。

use base::interface::exec::{ExecError, FileSystem};
use base::interface::permission::Subject;
use base::session::SessionId;
use history::blob::BlobStore;
use history::checkpoint::{
    After, Before, DelegateAttribution, PostRecord, PreFile, PreRecord, RecordedPath,
    CHECKPOINT_NS, MAX_FILE_BYTES, POST, PRE,
};
use history::entry::{EntryTags, LogEntry};
use history::store::HistoryStore;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Where a session's file tools record what they are about to overwrite.
pub struct Checkpoints {
    store: Arc<dyn HistoryStore>,
    root: SessionId,
    objects: Arc<dyn BlobStore>,
    project_root: Option<PathBuf>,
    author: Option<DelegateSeat>,
    locks: Arc<PathLocks>,
}

/// Which delegate of the root session a handle writes for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DelegateSeat {
    pub session_id: String,
    pub parent_tool_use_id: Option<String>,
    pub depth: u32,
}

/// One per file: a `RunCode` program's parallel calls and parallel delegates
/// can write the same file, and each one's `pre → write → post` must not
/// interleave with another's.
#[derive(Default)]
struct PathLocks(std::sync::Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>);

impl PathLocks {
    fn of(&self, path: &Path) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self.0.lock().unwrap_or_else(|e| e.into_inner());
        locks.retain(|_, lock| Arc::strong_count(lock) > 1);
        locks.entry(path.to_path_buf()).or_default().clone()
    }
}

/// Who wrote, for the `pre` line.
pub struct WriteBy<'a> {
    pub tool: &'a str,
    pub tool_use_id: Option<String>,
    pub rewind_id: Option<String>,
    /// The writing engine's turn, when it is in one.
    pub turn_id: Option<String>,
}

impl Checkpoints {
    /// The handle for a primary session, or `None` when its store keeps no
    /// copies.
    pub fn for_root(
        store: Arc<dyn HistoryStore>,
        root: &str,
        project_root: Option<PathBuf>,
    ) -> Option<Arc<Self>> {
        let root = SessionId::parse(root).ok()?;
        let objects = store.checkpoint_objects(root)?;
        Some(Arc::new(Self {
            store,
            root,
            objects,
            project_root,
            author: None,
            locks: Arc::default(),
        }))
    }

    /// The same log and copies, with the writes signed by a delegate.
    pub fn for_delegate(self: &Arc<Self>, seat: DelegateSeat) -> Arc<Self> {
        Arc::new(Self {
            store: self.store.clone(),
            root: self.root,
            objects: self.objects.clone(),
            project_root: self.project_root.clone(),
            author: Some(seat),
            locks: self.locks.clone(),
        })
    }

    pub fn root(&self) -> SessionId {
        self.root
    }

    pub fn project_root(&self) -> Option<&Path> {
        self.project_root.as_deref()
    }

    pub fn store(&self) -> &Arc<dyn HistoryStore> {
        &self.store
    }

    pub fn objects(&self) -> &Arc<dyn BlobStore> {
        &self.objects
    }

    /// Record the file a call whose subject is `subject` writes, before it
    /// does; `None` for a call that writes no file. An `Err` is the reason the
    /// call must not run: a write the session cannot roll back is a promise
    /// broken.
    pub(crate) async fn before_call(
        &self,
        subject: &Subject,
        by: WriteBy<'_>,
        tool_ctx: &base::tool::ToolContext,
    ) -> Result<Option<PendingWrite>, String> {
        let Subject::WritePath(raw) = subject else {
            return Ok(None);
        };
        let fs = tool_ctx.exec.filesystem.clone();
        let target = tools::security::resolve_write_target(raw, &tool_ctx.cwd, &*fs).await;
        let target = fs.canonicalize_best_effort(&target).await;
        self.before_write(fs, target, by).await.map(Some)
    }

    /// Record `target` as it is now, before something writes it.
    pub async fn before_write(
        &self,
        fs: Arc<dyn FileSystem>,
        target: PathBuf,
        by: WriteBy<'_>,
    ) -> Result<PendingWrite, String> {
        let lock = self.locks.of(&target).lock_owned().await;
        let refused = |why: String| {
            format!(
                "could not record a checkpoint before writing {}: {why}; nothing was written",
                target.display()
            )
        };
        let (before, path) = match RecordedPath::of(&target, self.project_root.as_deref()) {
            Some(path) => (observe(&*fs, &target, &*self.objects).await, path),
            None => (
                Ok(Before::Unreadable {
                    error: "unrepresentable path".into(),
                }),
                RecordedPath::Abs(target.to_string_lossy().into_owned()),
            ),
        };
        let before = before.map_err(|e| refused(e.to_string()))?;
        let id = self.store.environment().new_id().to_string();
        let (turn_id, agent) = match &self.author {
            None => (by.turn_id, None),
            Some(seat) => (
                None,
                Some(DelegateAttribution {
                    session_id: seat.session_id.clone(),
                    turn_id: by.turn_id,
                    parent_tool_use_id: seat.parent_tool_use_id.clone(),
                    depth: seat.depth,
                }),
            ),
        };
        let pre = PreRecord {
            id: id.clone(),
            files: vec![PreFile { path, before }],
            tool: by.tool.to_string(),
            tool_use_id: by.tool_use_id,
            rewind_id: by.rewind_id,
            agent,
        };
        let tags = EntryTags {
            turn_id,
            origin: None,
        };
        let payload = serde_json::to_value(&pre).map_err(|e| refused(e.to_string()))?;
        self.store
            .append_tagged(self.root, extension(PRE, payload), tags.clone())
            .await
            .map_err(|e| refused(e.to_string()))?;
        self.store
            .sync(self.root)
            .await
            .map_err(|e| refused(e.to_string()))?;
        Ok(PendingWrite {
            store: self.store.clone(),
            root: self.root,
            fs,
            target,
            pre: id,
            tags,
            _lock: lock,
        })
    }
}

fn extension(event: &str, payload: serde_json::Value) -> LogEntry {
    LogEntry::Extension {
        ns: CHECKPOINT_NS.into(),
        event: event.into(),
        payload,
    }
}

/// What `target` is now; a file's bytes go into `objects` first, so the
/// `pre` that names them is never ahead of them.
async fn observe(
    fs: &dyn FileSystem,
    target: &Path,
    objects: &dyn BlobStore,
) -> std::io::Result<Before> {
    let meta = match fs.metadata(target).await {
        Ok(meta) => meta,
        Err(ExecError::Denied(error)) => return Ok(Before::Unreadable { error }),
        Err(_) => return Ok(Before::Absent),
    };
    if !meta.is_file || meta.is_symlink {
        return Ok(Before::NotAFile);
    }
    let bytes = match fs.read(target).await {
        Ok(bytes) => bytes,
        Err(e) => {
            return Ok(Before::Unreadable {
                error: e.to_string(),
            })
        }
    };
    let len = bytes.len() as u64;
    if len > MAX_FILE_BYTES {
        return Ok(Before::TooLarge {
            sha256: history::checkpoint::sha256_hex(&bytes),
            bytes: len,
        });
    }
    let sha256 = objects.put(&bytes)?;
    Ok(Before::File { sha256, bytes: len })
}

/// What `target` holds now, as a `post` records it.
pub async fn current_state(fs: &dyn FileSystem, target: &Path) -> After {
    match fs.metadata(target).await {
        Ok(meta) if meta.is_file && !meta.is_symlink => match fs.read(target).await {
            Ok(bytes) => After::File {
                sha256: history::checkpoint::sha256_hex(&bytes),
            },
            Err(_) => After::Unknown,
        },
        Ok(_) | Err(ExecError::Denied(_)) => After::Unknown,
        Err(_) => After::Absent,
    }
}

/// A write that has its `pre`; [`finish`](Self::finish) writes its `post`.
/// The file's lock is held until then.
pub struct PendingWrite {
    store: Arc<dyn HistoryStore>,
    root: SessionId,
    fs: Arc<dyn FileSystem>,
    target: PathBuf,
    pre: String,
    tags: EntryTags,
    _lock: tokio::sync::OwnedMutexGuard<()>,
}

impl PendingWrite {
    pub fn target(&self) -> &Path {
        &self.target
    }

    /// A `post` that fails to land is only logged: the file is written and
    /// cannot be unwritten, and a rewind treats the path as unverified.
    pub async fn finish(self, ok: bool) -> After {
        let after = current_state(&*self.fs, &self.target).await;
        let post = PostRecord {
            pre: self.pre,
            ok,
            after: vec![after.clone()],
        };
        let written = match serde_json::to_value(&post) {
            Ok(payload) => self
                .store
                .append_tagged(self.root, extension(POST, payload), self.tags)
                .await
                .map_err(|e| e.to_string()),
            Err(e) => Err(e.to_string()),
        };
        if let Err(e) = written {
            tracing::warn!(error = %e, path = %self.target.display(), "failed to record a checkpoint's post");
        }
        after
    }
}
