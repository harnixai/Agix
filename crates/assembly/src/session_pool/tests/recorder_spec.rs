//! 带录制的会话：`agix.session/spec` 条目里的 recorder 怎么写、怎么读回、fork 时怎么改。

use super::mcp_for_project::test_pool_with_store;
use super::*;
use crate::session_pool::api::RecorderOptions;
use crate::session_pool::parts::specs::{recorded_in, SessionSpec};
use base::interface::model::{
    call_purpose, Model, ModelError, ModelEvent, ModelMessage, ModelStream, StreamParams, ToolDef,
    Usage,
};
use base::prompt::PromptBlock;
use base::provider::ApiType;
use history::store::{HistoryStore, InMemoryHistoryStore};

/// Answers every call: a naming call with `title`, anything else with a
/// line of text. Remembers what each call was for and which model it
/// asked for.
struct Answering {
    title: &'static str,
    calls: std::sync::Mutex<Vec<(Option<String>, String)>>,
}

impl Answering {
    fn new(title: &'static str) -> Arc<Self> {
        Arc::new(Self {
            title,
            calls: Default::default(),
        })
    }
}

#[async_trait::async_trait]
impl Model for Answering {
    fn api_type(&self) -> ApiType {
        ApiType::Anthropic
    }

    async fn stream(
        &self,
        _: Vec<PromptBlock>,
        _: Vec<ToolDef>,
        _: Vec<ModelMessage>,
        params: StreamParams,
        _: CancellationToken,
    ) -> Result<ModelStream, ModelError> {
        let purpose = params
            .origin
            .as_ref()
            .and_then(|o| o.purpose())
            .map(str::to_string);
        let text = if purpose.as_deref() == Some(call_purpose::TITLE) {
            self.title
        } else {
            "好的，这就整理。"
        };
        self.calls.lock().unwrap().push((purpose, params.model));
        let events = vec![
            Ok(ModelEvent::TextDelta { text: text.into() }),
            Ok(ModelEvent::EndTurn {
                stop_reason: "end_turn".into(),
                usage: Usage::default(),
            }),
        ];
        Ok(Box::new(futures::stream::iter(events)))
    }
}

/// Serve `project` on `scene` with `model`, and `task_router` when given,
/// in place of what its settings would build.
async fn bind(pool: &SessionPool, project: &Path, scene: &str, model: Arc<dyn Model>) {
    let mut settings = (*pool.bindings.pool_settings().await).clone();
    settings.values.memory.enabled = false;
    let mut binding = pool.bindings.binding_from(settings);
    binding.model = model;
    pool.bindings
        .insert((Some(project.to_path_buf()), scene.to_string()), binding)
        .await;
}

fn options(recorder: RecorderOptions) -> SessionOptions {
    SessionOptions {
        recorder: Some(recorder),
        model: Some("m-recorded".into()),
        ..Default::default()
    }
}

fn record(name: Option<&str>, dir: Option<&Path>) -> RecorderOptions {
    RecorderOptions {
        mode: "record".into(),
        name: name.map(str::to_string),
        dir: dir.map(|d| d.display().to_string()),
        strict: false,
    }
}

async fn recording_pool() -> (
    Arc<SessionPool>,
    Arc<InMemoryHistoryStore>,
    tempfile::TempDir,
    PathBuf,
) {
    let store = Arc::new(InMemoryHistoryStore::new());
    let (pool, dir) = test_pool_with_store(Some(store.clone())).await;
    let project = dir.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    bind(&pool, &project, "harnix", Answering::new("发布清单")).await;
    pool.activate_scene("harnix")
        .await
        .expect("harnix is built in");
    (pool, store, dir, project)
}

async fn create(pool: &SessionPool, project: &Path, options: &SessionOptions) -> String {
    pool.create_session(
        Some("harnix"),
        ProjectSelector::Path(project.to_path_buf()),
        Some(options),
    )
    .await
    .expect("a harnix session")
    .session_id
}

async fn one_turn(pool: &SessionPool, sid: &str) {
    let request = TurnRequest {
        session_id: Some(sid.to_string()),
        message: "把发布流程整理成一份清单".into(),
        attachments: vec![],
        turn_id: "t-1".into(),
        options: None,
        on_busy: OnBusy::Reject,
        initiator: Initiator::Client,
    };
    match pool.run_turn(request, None).await {
        Ok(TurnOutcome::Completed { .. }) => {}
        other => panic!("the turn did not complete: {other:?}"),
    }
}

async fn logged_spec(store: &InMemoryHistoryStore, sid: &str) -> SessionSpec {
    let entries = store
        .load(base::session::SessionId::parse(sid).unwrap())
        .await
        .unwrap();
    recorded_in(&entries)
}

fn logged_recorder(spec: &SessionSpec) -> Option<&RecorderOptions> {
    spec.options.as_ref()?.recorder.as_ref()
}

/// A recorder with no `dir` is logged with the absolute recordings root and no `name`, and records there.
#[tokio::test]
async fn a_recording_session_logs_its_recorder_with_the_root_frozen() {
    let (pool, store, _dir, project) = recording_pool().await;
    let sid = create(&pool, &project, &options(record(None, None))).await;

    let spec = logged_spec(&store, &sid).await;
    let recorder = logged_recorder(&spec).expect("a record-mode recorder is logged");
    assert_eq!(recorder.mode, "record");
    assert_eq!(recorder.name, None);
    let root = PathBuf::from(recorder.dir.as_deref().expect("the root is frozen"));
    assert!(root.is_absolute(), "{root:?}");

    one_turn(&pool, &sid).await;
    assert!(root.join(&sid).join("calls.jsonl").exists());
}

/// A replay recorder is not written to the log; the other options are.
#[tokio::test]
async fn a_replay_recorder_is_not_logged() {
    let (pool, store, dir, project) = recording_pool().await;
    let replay = RecorderOptions {
        mode: "replay".into(),
        name: Some("x".into()),
        dir: Some(dir.path().join("recordings").display().to_string()),
        strict: true,
    };
    let sid = create(&pool, &project, &options(replay)).await;

    let spec = logged_spec(&store, &sid).await;
    assert!(logged_recorder(&spec).is_none());
    assert_eq!(
        spec.options.and_then(|o| o.model).as_deref(),
        Some("m-recorded")
    );
}

/// A fork logs its source's recorder without the name, so it records under its own id.
#[tokio::test]
async fn a_fork_logs_the_recorder_without_its_name() {
    let (pool, store, dir, project) = recording_pool().await;
    let recordings = dir.path().join("recordings");
    let sid = create(
        &pool,
        &project,
        &options(record(Some("named-run"), Some(&recordings))),
    )
    .await;
    one_turn(&pool, &sid).await;

    let fork = pool.fork_session(&sid, None).await.expect("a fork");

    let forked = logged_spec(&store, &fork.session_id).await;
    let recorder = logged_recorder(&forked).expect("the fork records too");
    assert_eq!(recorder.mode, "record");
    assert_eq!(recorder.name, None);
    assert_eq!(
        recorder.dir.as_deref(),
        Some(recordings.display().to_string().as_str())
    );
    let source = logged_spec(&store, &sid).await;
    assert_eq!(
        logged_recorder(&source).and_then(|r| r.name.as_deref()),
        Some("named-run")
    );
}

/// A logged record-mode recorder wins over the recorder of the call that resumes the session.
#[tokio::test]
async fn a_logged_recording_wins_over_the_resuming_calls_recorder() {
    let (pool, store, dir, _project) = recording_pool().await;
    let sid = base::session::SessionId::new().to_string();
    let recordings = dir.path().join("recordings");
    let logged = SessionSpec {
        options: Some(options(record(Some("kept"), Some(&recordings)))),
        ..Default::default()
    };
    pool.specs.persist(&sid, &logged).await;
    let entries = store
        .load(base::session::SessionId::parse(&sid).unwrap())
        .await
        .unwrap();
    let replay = RecorderOptions {
        mode: "replay".into(),
        name: Some("other".into()),
        dir: Some(recordings.display().to_string()),
        strict: true,
    };

    let spec = pool
        .specs
        .spec_for(&sid, Some(&entries), Some(&options(replay)))
        .await;

    let recorder = logged_recorder(&spec).expect("a recorder");
    assert_eq!(recorder.mode, "record");
    assert_eq!(recorder.name.as_deref(), Some("kept"));
}
