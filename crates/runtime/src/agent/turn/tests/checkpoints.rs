//! 工具管线里的写前快照：文件工具的写留下 pre/post 与写前副本，被拒的调用不留，快照记不下来
//! 时调用不执行，子 Agent 的写记在根会话里。

use super::tool_exec::{
    local_hook_runner, test_exec_ctx_with_permission, AllowAllTestPermission, DenyAllTestPermission,
};
use crate::checkpoint::{Checkpoints, DelegateSeat};
use crate::turn::tool_exec::{execute_tool_with_telemetry, ToolExecCtx};
use base::context::Session;
use history::checkpoint::{plan, After, Before};
use history::store::{HistoryStore, InMemoryHistoryStore};
use std::sync::Arc;

struct Setup {
    dir: tempfile::TempDir,
    store: Arc<dyn HistoryStore>,
    root: base::session::SessionId,
    ctx: ToolExecCtx,
}

fn setup(
    store: Arc<dyn HistoryStore>,
    permission: Arc<dyn base::interface::permission::PermissionPolicy>,
) -> Setup {
    let dir = tempfile::tempdir().unwrap();
    let cwd = std::fs::canonicalize(dir.path()).unwrap();
    let tools = Arc::new(base::tool::InMemoryToolRegistry::new());
    tools.register(Arc::new(tools::file_write::FileWriteTool));
    let hooks = Arc::new(local_hook_runner(Default::default()));
    let mut ctx = test_exec_ctx_with_permission(tools, hooks, permission);
    ctx.cwd = cwd.clone();
    ctx.session_state = Arc::new(session_state::SessionState::for_tests(cwd.clone()));
    let root = base::session::SessionId::new();
    ctx.checkpoints = Checkpoints::for_root(
        store.clone(),
        &root.to_string(),
        Some(cwd),
        Default::default(),
    );
    Setup {
        dir,
        store,
        root,
        ctx,
    }
}

fn write(path: &std::path::Path, content: &str) -> serde_json::Value {
    serde_json::json!({ "file_path": path.to_string_lossy(), "content": content })
}

impl Setup {
    fn path(&self, name: &str) -> std::path::PathBuf {
        self.ctx.cwd.join(name)
    }

    async fn pres(
        &self,
    ) -> Vec<(
        history::entry::EnvelopedEntry,
        history::checkpoint::PreRecord,
    )> {
        match self.store.load(self.root).await {
            Ok(entries) => entries
                .into_iter()
                .filter_map(|e| plan::pre_of(&e).map(|p| (e, p)))
                .collect(),
            Err(_) => Vec::new(),
        }
    }
}

#[tokio::test]
async fn a_write_leaves_a_pre_and_a_post_and_keeps_what_was_there() {
    let s = setup(
        Arc::new(InMemoryHistoryStore::new()),
        Arc::new(AllowAllTestPermission),
    );
    let target = s.path("a.txt");
    std::fs::write(&target, "v1").unwrap();
    s.ctx
        .session_state
        .record_read(base::context::ReadRecord::whole(&target, "toolu_read"));

    execute_tool_with_telemetry(&s.ctx, "Write", write(&target, "v2"))
        .await
        .expect("written");
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "v2");

    let pres = s.pres().await;
    assert_eq!(pres.len(), 1);
    let (env, pre) = &pres[0];
    assert_eq!(env.turn_id.as_deref(), Some("test-turn"));
    assert_eq!(pre.tool, "Write");
    assert_eq!(pre.tool_use_id.as_deref(), Some("toolu_test"));
    let Before::File { sha256, .. } = &pre.files[0].before else {
        panic!("{:?}", pre.files[0].before);
    };
    let kept = s
        .store
        .checkpoint_objects(s.root)
        .unwrap()
        .get(sha256)
        .unwrap();
    assert_eq!(kept.as_deref(), Some(&b"v1"[..]));
    let steps = plan::file_steps(&s.store.load(s.root).await.unwrap(), 0);
    assert_eq!(steps[0].path.as_str(), "a.txt");
    assert_eq!(
        steps[0].expected_now,
        Some(After::File {
            sha256: history::checkpoint::sha256_hex(b"v2")
        })
    );
}

#[tokio::test]
async fn a_refused_call_leaves_no_checkpoint() {
    let s = setup(
        Arc::new(InMemoryHistoryStore::new()),
        Arc::new(DenyAllTestPermission),
    );
    let target = s.path("new.txt");
    assert!(
        execute_tool_with_telemetry(&s.ctx, "Write", write(&target, "x"))
            .await
            .is_err()
    );
    assert!(s.pres().await.is_empty());
    assert!(!target.exists());
}

/// Refuses every append.
#[derive(Default)]
struct FullDisk(InMemoryHistoryStore);

#[async_trait::async_trait]
impl HistoryStore for FullDisk {
    async fn append(
        &self,
        _: base::session::SessionId,
        _: history::entry::LogEntry,
    ) -> Result<(), history::error::HistoryError> {
        Err(history::error::HistoryError::Io(std::io::Error::other(
            "no space left on device",
        )))
    }
    async fn load(
        &self,
        session: base::session::SessionId,
    ) -> Result<Vec<history::entry::EnvelopedEntry>, history::error::HistoryError> {
        self.0.load(session).await
    }
    async fn list_sessions(
        &self,
    ) -> Result<Vec<base::session::SessionId>, history::error::HistoryError> {
        self.0.list_sessions().await
    }
    async fn delete(
        &self,
        session: base::session::SessionId,
    ) -> Result<(), history::error::HistoryError> {
        self.0.delete(session).await
    }
    fn checkpoint_objects(
        &self,
        root: base::session::SessionId,
    ) -> Option<Arc<dyn history::blob::BlobStore>> {
        self.0.checkpoint_objects(root)
    }
}

#[tokio::test]
async fn a_write_whose_checkpoint_cannot_be_recorded_does_not_run() {
    let s = setup(
        Arc::new(FullDisk::default()),
        Arc::new(AllowAllTestPermission),
    );
    let target = s.path("a.txt");
    std::fs::write(&target, "v1").unwrap();
    s.ctx
        .session_state
        .record_read(base::context::ReadRecord::whole(&target, "toolu_read"));
    let refused = execute_tool_with_telemetry(&s.ctx, "Write", write(&target, "v2"))
        .await
        .expect_err("refused");
    assert!(
        refused.contains("could not record a checkpoint")
            && refused.contains("nothing was written"),
        "{refused}"
    );
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "v1");
}

#[tokio::test]
async fn a_delegates_write_is_in_the_root_log_under_its_name_and_no_turn_of_the_root() {
    let mut s = setup(
        Arc::new(InMemoryHistoryStore::new()),
        Arc::new(AllowAllTestPermission),
    );
    let root = s.ctx.checkpoints.clone().unwrap();
    s.ctx.checkpoints = Some(root.for_delegate(DelegateSeat {
        session_id: "sub-1".into(),
        parent_tool_use_id: Some("toolu_agent".into()),
        depth: 1,
    }));
    s.ctx.turn_id = "sub-turn".into();
    let target = s.path("c.txt");
    execute_tool_with_telemetry(&s.ctx, "Write", write(&target, "c"))
        .await
        .expect("written");
    let pres = s.pres().await;
    let (env, pre) = &pres[0];
    assert_eq!(env.turn_id, None);
    assert_eq!(pre.files[0].before, Before::Absent);
    let agent = pre.agent.as_ref().expect("signed by the delegate");
    assert_eq!(agent.session_id, "sub-1");
    assert_eq!(agent.turn_id.as_deref(), Some("sub-turn"));
    assert_eq!(agent.parent_tool_use_id.as_deref(), Some("toolu_agent"));
    drop(s.dir);
}
