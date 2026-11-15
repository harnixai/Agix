//! Shared harness for the daemon's integration tests: scripted model rounds,
//! a scripted-model daemon on a real Unix socket, one-shot JSON-RPC calls,
//! and the global-tier trust a case sets up when it is not about trust.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use daemon::config::{load_daemon_config, StaticDaemonPaths};
use daemon::DaemonServer;
use model::client::{AnthropicClient, AuthMode, CountFuture, EventStream, HttpAnthropicClient};
use model::stream::{
    BlockDelta, ContentBlockStart, MessageDeltaPayload, StreamEvent, Usage as WireUsage,
};
use model::types::MessagesRequest;
use settings::Settings;
#[cfg(feature = "plugin-packages")]
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio_util::sync::CancellationToken;

/// An `AnthropicClient` that replays one canned SSE-event batch per call, in
/// order, then repeats the last one forever — so an unexpected extra round
/// trip ends the turn instead of hanging the test.
pub struct ScriptedClient {
    rounds: StdMutex<std::collections::VecDeque<Vec<StreamEvent>>>,
    last: StdMutex<Vec<StreamEvent>>,
    /// Every request the engine made, for tests that assert on what the
    /// model was actually shown (e.g. "did the fork carry the transcript?").
    seen: Arc<StdMutex<Vec<MessagesRequest>>>,
}

pub type SeenRequests = Arc<StdMutex<Vec<MessagesRequest>>>;

impl ScriptedClient {
    pub fn new(rounds: Vec<Vec<StreamEvent>>) -> (Arc<Self>, SeenRequests) {
        let seen: SeenRequests = Arc::new(StdMutex::new(Vec::new()));
        let last = rounds.last().cloned().unwrap_or_else(|| text_round("done"));
        (
            Arc::new(Self {
                rounds: StdMutex::new(rounds.into()),
                last: StdMutex::new(last),
                seen: seen.clone(),
            }),
            seen,
        )
    }
}

impl AnthropicClient for ScriptedClient {
    fn stream_messages(&self, req: MessagesRequest) -> EventStream {
        self.seen.lock().unwrap().push(req);
        let events = {
            let mut q = self.rounds.lock().unwrap();
            q.pop_front()
                .unwrap_or_else(|| self.last.lock().unwrap().clone())
        };
        if events.is_empty() {
            return Box::pin(futures::stream::pending());
        }
        Box::pin(futures::stream::iter(events.into_iter().map(Ok)))
    }

    fn count_tokens<'a>(&'a self, _req: &'a MessagesRequest) -> CountFuture<'a> {
        Box::pin(async { Ok(0usize) })
    }
}

/// One assistant turn that says `text` and stops.
pub fn text_round(text: &str) -> Vec<StreamEvent> {
    vec![
        StreamEvent::ContentBlockStart {
            index: 0,
            content_block: ContentBlockStart::Text {
                text: String::new(),
            },
        },
        StreamEvent::ContentBlockDelta {
            index: 0,
            delta: BlockDelta::TextDelta {
                text: text.to_string(),
            },
        },
        StreamEvent::ContentBlockStop { index: 0 },
        StreamEvent::MessageDelta {
            delta: MessageDeltaPayload {
                stop_reason: Some(base::message::StopReason::EndTurn),
                stop_sequence: None,
            },
            usage: Some(WireUsage::default()),
        },
    ]
}

/// The same, reporting what the call cost.
///
/// A turn is several calls and a provider reports usage per call, so a round
/// that always reports zero cannot tell a sum from a last value — which is the
/// whole question for anyone budgeting on the number.
pub fn text_round_costing(text: &str, input_tokens: u64, output_tokens: u64) -> Vec<StreamEvent> {
    let mut round = text_round(text);
    if let Some(StreamEvent::MessageDelta { usage, .. }) = round.last_mut() {
        *usage = Some(WireUsage {
            input_tokens,
            output_tokens,
            ..Default::default()
        });
    }
    round
}

/// A tool round that reports what it cost, for the same reason.
pub fn tool_round_costing(
    tool: &str,
    id: &str,
    input: serde_json::Value,
    input_tokens: u64,
    output_tokens: u64,
) -> Vec<StreamEvent> {
    let mut round = tool_round(tool, id, input);
    if let Some(StreamEvent::MessageDelta { usage, .. }) = round.last_mut() {
        *usage = Some(WireUsage {
            input_tokens,
            output_tokens,
            ..Default::default()
        });
    }
    round
}

/// One assistant turn that calls `tool` with `input` and stops on `tool_use`.
pub fn tool_round(tool: &str, id: &str, input: serde_json::Value) -> Vec<StreamEvent> {
    vec![
        StreamEvent::ContentBlockStart {
            index: 0,
            content_block: ContentBlockStart::ToolUse {
                id: id.to_string(),
                name: tool.to_string(),
                input: serde_json::Value::Null,
            },
        },
        StreamEvent::ContentBlockDelta {
            index: 0,
            delta: BlockDelta::InputJsonDelta {
                partial_json: input.to_string(),
            },
        },
        StreamEvent::ContentBlockStop { index: 0 },
        StreamEvent::MessageDelta {
            delta: MessageDeltaPayload {
                stop_reason: Some(base::message::StopReason::ToolUse),
                stop_sequence: None,
            },
            usage: Some(WireUsage::default()),
        },
    ]
}

/// A turn where the model calls `Bash` with a command the classifier can't
/// vouch for — `touch` is in neither the read-only nor the destructive list,
/// so `Bash` claims nothing about it and the policy falls through
/// to mode dispatch, which is exactly the path the new default changes.
pub fn asking_bash_rounds(marker: &str) -> Vec<Vec<StreamEvent>> {
    vec![
        tool_round(
            "Bash",
            "tool-1",
            serde_json::json!({"command": format!("touch {marker}")}),
        ),
        text_round("all done"),
    ]
}

pub struct ScriptedServer {
    pub server: Arc<DaemonServer>,
    pub sock: PathBuf,
    pub _dir: tempfile::TempDir,
    pub handle: tokio::task::JoinHandle<()>,
    pub store: Arc<dyn history::store::HistoryStore>,
}

impl ScriptedServer {
    pub async fn stop(self) {
        self.server.shutdown_token().cancel();
        let _ = self.handle.await;
    }
}

/// A daemon whose model is scripted, with a real `HistoryStore` (handed back
/// so tests can inspect transcripts directly) and caller-supplied `Settings`
/// so the permission posture under test is explicit rather than ambient.
/// What a scripted server starts from: the caller's settings, and the
/// permission rules a settings file would have added.
pub struct Seed(pub Settings, pub Vec<base::permission::PermissionRuleEntry>);

impl From<Settings> for Seed {
    fn from(settings: Settings) -> Self {
        Self(settings, Vec::new())
    }
}

pub async fn start_scripted_server(
    rounds: Vec<Vec<StreamEvent>>,
    settings: impl Into<Seed>,
    prompt_timeout: Duration,
) -> (ScriptedServer, SeenRequests) {
    start_scripted_server_in("code", rounds, settings, prompt_timeout).await
}

/// The same daemon in a named scene — `harnix` names its own sessions after the
/// first turn, which `code` does not, and that difference is a behaviour
/// worth a test.
pub async fn start_scripted_server_in(
    scene_id: &str,
    rounds: Vec<Vec<StreamEvent>>,
    settings: impl Into<Seed>,
    prompt_timeout: Duration,
) -> (ScriptedServer, SeenRequests) {
    start_scripted_server_holding(scene_id, rounds, settings, prompt_timeout, 8).await
}

/// The same daemon keeping at most `session_cap` sessions in memory.
pub async fn start_scripted_server_holding(
    scene_id: &str,
    rounds: Vec<Vec<StreamEvent>>,
    settings: impl Into<Seed>,
    prompt_timeout: Duration,
    session_cap: usize,
) -> (ScriptedServer, SeenRequests) {
    // Silent unless `RUST_LOG` says otherwise. These tests drive the whole
    // engine, so being able to turn logging on without editing the file is
    // worth the four lines — it is how the `respondToPrompt` deadlock this
    // module's `permission_prompt` route works around was diagnosed.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("off")),
        )
        .try_init();
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("test.sock");
    let cwd = dir.path().join("work");
    std::fs::create_dir_all(&cwd).unwrap();

    let (client, seen) = ScriptedClient::new(rounds);
    let mut config = load_daemon_config(
        "claude-sonnet-4-6",
        2000,
        Some(&sock),
        scene_id,
        &StaticDaemonPaths::new(dir.path().to_path_buf()),
    );
    config.session_cap = session_cap;
    let Seed(settings, rules) = settings.into();
    config.settings.values = settings;
    config.settings.rules = rules
        .into_iter()
        .map(|rule| {
            base::permission::LoadedRule::new(
                rule,
                base::interface::config_source::Tier::Project,
                false,
            )
        })
        .collect();

    let store: Arc<dyn history::store::HistoryStore> = Arc::new(
        history::store::JsonlHistoryStore::with_roots(
            &cwd,
            history::path::HistoryRoots::under(dir.path()),
        )
        .await
        .unwrap(),
    );

    // The entry point `main.rs` uses: these tests are about the behaviour of
    // an assembled daemon, and an assembly of their own would be one more
    // copy to keep in step with it.
    let mut registry = scene::SceneRegistry::new();
    registry.register_builtin();
    let agent_scene = registry.resolve(scene_id).expect("a builtin scene");

    let pool = daemon::assemble::pool(
        &config,
        agent_scene,
        daemon::Assembly {
            cwd: Some(cwd.clone()),
            permission_prompt_timeout: Some(prompt_timeout),
            model_client: Some(client),
            transcripts: daemon::Transcripts::In(store.clone()),
            ..Default::default()
        },
    )
    .await
    .expect("the daemon assembles");

    let cancel = CancellationToken::new();
    let server = Arc::new(DaemonServer::new(pool, cancel));
    let server2 = server.clone();
    let sock2 = sock.clone();
    let handle = tokio::spawn(async move {
        let _ = server2.serve_unix(&sock2).await;
    });
    for _ in 0..60 {
        if sock.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(sock.exists(), "socket never bound");

    (
        ScriptedServer {
            server,
            sock,
            _dir: dir,
            handle,
            store,
        },
        seen,
    )
}

/// The same daemon shape but with `history_store: None` — the degraded mode
/// `main.rs` falls into when `JsonlHistoryStore::with_root` fails at
/// startup, where the session read/branch surface has no possible answer.
pub async fn start_server_without_history() -> (
    Arc<DaemonServer>,
    PathBuf,
    tempfile::TempDir,
    tokio::task::JoinHandle<()>,
) {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("test.sock");
    let (client, _seen) = ScriptedClient::new(vec![text_round("x")]);
    let config = load_daemon_config(
        "claude-sonnet-4-6",
        2000,
        Some(&sock),
        "code",
        &StaticDaemonPaths::new(dir.path().to_path_buf()),
    );

    let pool = daemon::assemble::pool(
        &config,
        Arc::new(scene::code::spec(scene::code::Interaction::Interactive)),
        daemon::Assembly {
            cwd: Some(dir.path().to_path_buf()),
            model_client: Some(client),
            transcripts: daemon::Transcripts::Nowhere,
            ..Default::default()
        },
    )
    .await
    .expect("the daemon assembles");

    let cancel = CancellationToken::new();
    let server = Arc::new(DaemonServer::new(pool, cancel));
    let server2 = server.clone();
    let sock2 = sock.clone();
    let handle = tokio::spawn(async move {
        let _ = server2.serve_unix(&sock2).await;
    });
    for _ in 0..60 {
        if sock.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(sock.exists(), "socket never bound");
    (server, sock, dir, handle)
}

/// One request, one response line (for methods that don't stream).
pub async fn rpc(sock: &std::path::Path, msg: &str) -> serde_json::Value {
    let mut client = UnixStream::connect(sock).await.unwrap();
    client.write_all(msg.as_bytes()).await.unwrap();
    client.write_all(b"\n").await.unwrap();
    let (r, _) = client.split();
    let mut br = BufReader::new(r);
    let mut buf = String::new();
    br.read_line(&mut buf).await.unwrap();
    serde_json::from_str(&buf).unwrap()
}

/// Send one request and drain the connection until the final response (the
/// line carrying an `id`), returning `(stream_frames, response)`.
pub async fn rpc_streaming(
    sock: &std::path::Path,
    msg: &str,
) -> (Vec<serde_json::Value>, serde_json::Value) {
    let mut client = UnixStream::connect(sock).await.unwrap();
    client.write_all(msg.as_bytes()).await.unwrap();
    client.write_all(b"\n").await.unwrap();
    let (r, _w) = client.split();
    let mut br = BufReader::new(r);
    let mut frames = Vec::new();
    loop {
        let mut line = String::new();
        let n = br.read_line(&mut line).await.unwrap();
        assert!(n > 0, "connection closed before a final response arrived");
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        if v.get("id").is_some() {
            return (frames, v);
        }
        frames.push(v);
    }
}

pub fn events_of_kind<'a>(
    frames: &'a [serde_json::Value],
    kind: &str,
) -> Vec<&'a serde_json::Value> {
    frames
        .iter()
        .filter(|f| f["params"]["event"]["kind"] == kind)
        .map(|f| &f["params"]["event"])
        .collect()
}

/// Run one turn to completion, returning the session id.
pub async fn run_turn(sock: &std::path::Path, session_id: Option<&str>, message: &str) -> String {
    let params = match session_id {
        Some(sid) => format!(r#"{{"session_id":"{sid}","message":"{message}"}}"#),
        None => format!(r#"{{"message":"{message}"}}"#),
    };
    let (_frames, resp) = rpc_streaming(
        sock,
        &format!(r#"{{"jsonrpc":"2.0","method":"session.run_turn","params":{params},"id":1}}"#),
    )
    .await;
    assert!(resp["result"].is_object(), "turn failed: {resp}");
    resp["result"]["session_id"].as_str().unwrap().to_string()
}

pub fn stop(reason: base::message::StopReason) -> StreamEvent {
    StreamEvent::MessageDelta {
        delta: MessageDeltaPayload {
            stop_reason: Some(reason),
            stop_sequence: None,
        },
        usage: Some(WireUsage::default()),
    }
}

pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}

/// One request, its whole response (result or error), the stream frames in
/// between dropped.
pub async fn call_frame(sock: &Path, method: &str, params: serde_json::Value) -> serde_json::Value {
    let mut client = rpc_client::DaemonRpcClient::connect(sock).await.unwrap();
    let response = client.call(method, params).await.unwrap();
    serde_json::to_value(response).unwrap()
}

/// One request that must succeed, answered with its `result`.
pub async fn call(sock: &Path, method: &str, params: serde_json::Value) -> serde_json::Value {
    let frame = call_frame(sock, method, params).await;
    assert!(frame.get("result").is_some(), "{method} failed: {frame}");
    frame["result"].clone()
}

pub struct PermitAll;

#[async_trait::async_trait]
impl base::interface::permission::PermissionPolicy for PermitAll {
    async fn decide(
        &self,
        _req: &base::interface::permission::PermissionRequest<'_>,
    ) -> base::interface::permission::PermissionOutcome {
        base::interface::permission::PermissionOutcome::Permit {
            basis: base::interface::permission::Basis::Bypass,
            profile: Default::default(),
            obligations: Default::default(),
        }
    }
}

/// What a daemon serving a caller-supplied model client starts from.
pub struct Serving {
    pub scene: &'static str,
    pub model: &'static str,
    pub max_tokens: u32,
    pub session_cap: Option<usize>,
    pub history: bool,
}

impl Default for Serving {
    fn default() -> Self {
        Self {
            scene: "code",
            model: "claude-sonnet-4-6",
            max_tokens: 2000,
            session_cap: None,
            history: false,
        }
    }
}

pub struct Served {
    pub sock: PathBuf,
    pub work: PathBuf,
    pub global: PathBuf,
    pub store: Option<Arc<dyn history::store::HistoryStore>>,
    pub dir: tempfile::TempDir,
    pub handle: tokio::task::JoinHandle<()>,
}

/// A daemon on a real socket whose permission policy permits everything,
/// memory extraction off, serving `client`; `configure` edits the config
/// before the pool is assembled.
pub async fn serve_permitting_all(
    client: Arc<dyn AnthropicClient>,
    serving: Serving,
    configure: impl FnOnce(&mut daemon::config::DaemonConfig),
) -> Served {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("daemon.sock");
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work).unwrap();

    let mut config = load_daemon_config(
        serving.model,
        serving.max_tokens,
        Some(&sock),
        serving.scene,
        &StaticDaemonPaths::new(dir.path().to_path_buf()),
    );
    if let Some(cap) = serving.session_cap {
        config.session_cap = cap;
    }
    config.settings.values.memory_enabled = false;
    configure(&mut config);

    let global = config.settings.paths.global_data_dir.clone();
    let store: Option<Arc<dyn history::store::HistoryStore>> = if serving.history {
        Some(Arc::new(
            history::store::JsonlHistoryStore::with_roots(
                &work,
                history::path::HistoryRoots::under(&global),
            )
            .await
            .unwrap(),
        ))
    } else {
        None
    };
    let scene = if serving.scene == "harnix" {
        Arc::new(scene::harnix::spec())
    } else {
        Arc::new(scene::code::spec(scene::code::Interaction::Interactive))
    };

    let pool = daemon::assemble::pool(
        &config,
        scene,
        daemon::Assembly {
            cwd: Some(work.clone()),
            model_client: Some(client),
            transcripts: match &store {
                Some(store) => daemon::Transcripts::In(store.clone()),
                None => daemon::Transcripts::default(),
            },
            host_policy: Some(Arc::new(PermitAll)),
            ..Default::default()
        },
    )
    .await
    .expect("the daemon assembles");

    let server = Arc::new(DaemonServer::new(pool, CancellationToken::new()));
    let serving_server = server.clone();
    let bind = sock.clone();
    let handle = tokio::spawn(async move {
        let _ = serving_server.serve_unix(&bind).await;
    });
    for _ in 0..80 {
        if sock.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(sock.exists(), "socket never bound");

    Served {
        sock,
        work,
        global,
        store,
        dir,
        handle,
    }
}

/// A connection of its own, for tests that read frames as they arrive.
pub async fn open(sock: &Path) -> rpc_client::DaemonRpcClient {
    rpc_client::DaemonRpcClient::connect(sock).await.unwrap()
}

/// One request; every frame before the response, then the response frame.
pub async fn call_streaming(
    sock: &Path,
    method: &str,
    params: serde_json::Value,
) -> (Vec<serde_json::Value>, serde_json::Value) {
    let mut client = open(sock).await;
    let id = client.send_request(method, params).await.unwrap();
    let mut frames = Vec::new();
    loop {
        let frame = client
            .next_frame()
            .await
            .unwrap()
            .unwrap_or_else(|| panic!("connection closed before a response to {method}"));
        if frame.get("id") == Some(&serde_json::json!(id)) {
            return (frames, frame);
        }
        frames.push(frame);
    }
}

/// A connection that has sent `method` and seen it acknowledged; whatever the
/// daemon pushes afterwards is read with `next_frame`.
pub async fn subscribed(
    sock: &Path,
    method: &str,
    params: serde_json::Value,
) -> rpc_client::DaemonRpcClient {
    let mut client = open(sock).await;
    let response = client.call(method, params).await.unwrap();
    assert!(response.error.is_none(), "{method} refused: {response:?}");
    client
}

/// Subscribe on a connection of its own and collect what `pick` keeps of
/// every frame the daemon pushes afterwards.
pub async fn watch(
    sock: &Path,
    method: &str,
    params: serde_json::Value,
    pick: impl Fn(&serde_json::Value) -> Option<serde_json::Value> + Send + 'static,
) -> Arc<StdMutex<Vec<serde_json::Value>>> {
    let seen: Arc<StdMutex<Vec<serde_json::Value>>> = Arc::default();
    let mut client = subscribed(sock, method, params).await;
    let into = seen.clone();
    tokio::spawn(async move {
        while let Ok(Some(frame)) = client.next_frame().await {
            if let Some(kept) = pick(&frame) {
                into.lock().unwrap().push(kept);
            }
        }
    });
    seen
}

/// A connection with a request in flight, the way a turn is started.
pub async fn begin(
    sock: &Path,
    method: &str,
    params: serde_json::Value,
) -> rpc_client::DaemonRpcClient {
    let mut client = open(sock).await;
    client.send_request(method, params).await.unwrap();
    client
}

/// Trust `root` and every project under it, keeping whatever else the
/// global `settings.local.json` in `global` says.
pub fn trust_projects_under(global: &Path, root: &Path) {
    let path = global.join("settings.local.json");
    let mut local: serde_json::Value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    let trusted = local["project_trust"]["trusted"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .chain([serde_json::json!(root), serde_json::json!(root.join("**"))])
        .collect::<Vec<_>>();
    local["project_trust"]["trusted"] = serde_json::Value::Array(trusted);
    std::fs::create_dir_all(global).unwrap();
    std::fs::write(&path, local.to_string()).unwrap();
}

pub async fn history(sock: &std::path::Path, sid: &str) -> serde_json::Value {
    rpc(
        sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.history","params":{{"session_id":"{sid}","limit":500}},"id":9}}"#
        ),
    )
    .await["result"]
        .clone()
}

/// `Settings` at defaults — notably `permission_mode` unset, which is
/// `Default` (= ask), the posture under test.
pub fn ask_settings() -> Settings {
    Settings::defaults_for("claude-sonnet-4-6")
}

/// `ask_settings()` with post-turn memory extraction switched off — for
/// tests that inspect the *last* captured `MessagesRequest` and would
/// otherwise pick up the extraction call's fixed cheap model instead of the
/// main turn's.
pub fn ask_settings_no_memory() -> Settings {
    let mut s = ask_settings();
    s.memory_enabled = false;
    s
}

pub async fn close_and_resume(sock: &std::path::Path, sid: &str) {
    rpc(
        sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.close","id":5,"params":{{"session_id":"{sid}"}}}}"#
        ),
    )
    .await;
    let resumed = rpc(
        sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.resume","id":6,"params":{{"session_id":"{sid}"}}}}"#
        ),
    )
    .await;
    assert_eq!(resumed["result"]["status"], "resumed", "{resumed}");
}

/// A transcript recorded under `scene`, with nothing in it but its `Meta`
/// line — enough for the check in `docs/daemon_rpc_protocol.md` §3.4, which reads that line and nothing else.
pub async fn inject_session_in_scene(
    srv: &ScriptedServer,
    scene: &str,
) -> base::session::SessionId {
    let sid = base::session::SessionId::new();
    srv.store
        .append(
            sid,
            history::entry::LogEntry::Meta {
                cwd: "/tmp".into(),
                started_at: time::OffsetDateTime::now_utc(),
                model: "claude-sonnet-4-6".into(),
                permission_mode: "default".into(),
                engine_version: "0.0.1".into(),
                agix_version: "0.0.1".into(),
                parent_session_id: None,
                scene: Some(scene.to_string()),
                project_root: None,
                session_kind: history::entry::SessionKind::Primary,
                schema_version: history::entry::CURRENT_META_SCHEMA_VERSION,
            },
        )
        .await
        .unwrap();
    sid
}

/// Read `client` until the response to request `id`, returning the frames
/// that came before it and the response.
pub async fn until_response(
    client: &mut rpc_client::DaemonRpcClient,
    id: i64,
) -> (Vec<serde_json::Value>, serde_json::Value) {
    let mut frames = Vec::new();
    loop {
        let v = client
            .next_frame()
            .await
            .unwrap()
            .expect("the connection closed before the response");
        if v["id"] == id {
            return (frames, v);
        }
        frames.push(v);
    }
}

pub async fn inject_sidechain(srv: &ScriptedServer, parent: &str) -> base::session::SessionId {
    let child_sid = base::session::SessionId::new();
    srv.store
        .append(
            child_sid,
            history::entry::LogEntry::Meta {
                cwd: "/tmp".into(),
                started_at: time::OffsetDateTime::now_utc(),
                model: "claude-sonnet-4-6".into(),
                permission_mode: "default".into(),
                engine_version: "0.0.1".into(),
                agix_version: "0.0.1".into(),
                parent_session_id: Some(parent.to_string()),
                scene: Some("code".into()),
                project_root: None,
                session_kind: history::entry::SessionKind::Sidechain,
                schema_version: history::entry::CURRENT_META_SCHEMA_VERSION,
            },
        )
        .await
        .unwrap();
    child_sid
}

/// Drive a turn on one connection while answering prompts on another — the
/// turn's own connection is blocked streaming frames, which is exactly the
/// shape a real client is in when a prompt arrives. `answer` is called once
/// per prompt with the prompt's `event` and returns the JSON to send back —
/// the `decision` for a permission prompt, the `answer` for a question — or
/// `None` to leave the prompt unanswered.
pub async fn run_turn_answering_prompts<F>(
    sock: &std::path::Path,
    message: &str,
    options: &str,
    mut answer: F,
) -> (Vec<serde_json::Value>, serde_json::Value)
where
    F: FnMut(&serde_json::Value) -> Option<String>,
{
    let request = format!(
        r#"{{"jsonrpc":"2.0","method":"session.run_turn","params":{{"message":"{message}"{options}}},"id":1}}"#
    );
    let mut conn = UnixStream::connect(sock).await.unwrap();
    conn.write_all(request.as_bytes()).await.unwrap();
    conn.write_all(b"\n").await.unwrap();
    let (r, _w) = conn.split();
    let mut br = BufReader::new(r);

    let mut frames = Vec::new();
    loop {
        let mut line = String::new();
        let n = br.read_line(&mut line).await.unwrap();
        assert!(n > 0, "connection closed before a final response arrived");
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        if v.get("id").is_some() {
            return (frames, v);
        }
        if v["params"]["event"]["kind"] == "prompt" {
            if let Some(reply) = answer(&v["params"]["event"]) {
                let sid = v["params"]["session_id"].as_str().unwrap();
                let prompt_id = v["params"]["event"]["prompt_id"].as_str().unwrap();
                let field = if v["params"]["event"]["prompt_type"] == "question" {
                    r#""prompt_type":"question","answer""#
                } else {
                    r#""decision""#
                };
                let ack = rpc(
                    sock,
                    &format!(
                        r#"{{"jsonrpc":"2.0","method":"session.respondToPrompt","params":{{"session_id":"{sid}","prompt_id":"{prompt_id}",{field}:{reply}}},"id":99}}"#
                    ),
                )
                .await;
                assert_eq!(ack["result"]["prompt_id"], prompt_id, "{ack}");
            }
        }
        frames.push(v);
    }
}

pub fn hooked_settings(hooks: serde_json::Value) -> Settings {
    let mut s = ask_settings_no_memory();
    s.hooks_config = Some(hooks);
    s
}

/// Runs a turn on `sid` (or a new session) and hands back the
/// `turn_complete` frame, failing if the turn never produced one.
pub async fn turn_complete_of(
    sock: &std::path::Path,
    session_id: Option<&str>,
    message: &str,
) -> (String, serde_json::Value) {
    let params = match session_id {
        Some(sid) => format!(r#"{{"session_id":"{sid}","message":"{message}"}}"#),
        None => format!(r#"{{"message":"{message}"}}"#),
    };
    let (frames, resp) = tokio::time::timeout(
        Duration::from_secs(20),
        rpc_streaming(
            sock,
            &format!(r#"{{"jsonrpc":"2.0","method":"session.run_turn","params":{params},"id":1}}"#),
        ),
    )
    .await
    .expect("the turn never ended");
    assert!(resp["result"].is_object(), "turn failed: {resp}");
    let done = events_of_kind(&frames, "turn_complete");
    assert_eq!(done.len(), 1, "exactly one turn_complete: {frames:?}");
    (
        resp["result"]["session_id"].as_str().unwrap().to_string(),
        done[0].clone(),
    )
}

pub async fn permit(sock: &std::path::Path, sid: &str, prompt_id: &str) {
    let ack = rpc(
        sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.respondToPrompt","params":{{"session_id":"{sid}","prompt_id":"{prompt_id}","decision":{{"type":"permit"}}}},"id":99}}"#
        ),
    )
    .await;
    assert_eq!(ack["result"]["prompt_id"], prompt_id, "{ack}");
}

/// The transcript directory `project_root` names, resolved the way the store
/// resolves it (macOS temp directories are symlinks, so the sanitized name is
/// built from the canonical path or from nothing at all).
pub fn transcript_dir(global_root: &Path, project: Option<&Path>) -> PathBuf {
    let canonical = project.map(|p| std::fs::canonicalize(p).unwrap());
    history::path::project_dir_for(&global_root.join("projects"), canonical.as_deref())
}

/// Build a zip archive (in memory) around one `plugin.toml`, write it to
/// `dir`, and return `(archive_path, sha256_hex)`.
#[cfg(feature = "plugin-packages")]
pub fn build_plugin_zip(
    dir: &std::path::Path,
    plugin_name: &str,
    manifest: &str,
) -> (PathBuf, String) {
    let mut buf = Vec::new();
    {
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        let opts: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default();
        writer.start_file("plugin.toml", opts).unwrap();
        use std::io::Write;
        write!(writer, "{manifest}").unwrap();
        writer.finish().unwrap();
    }
    let archive_path = dir.join(format!("{plugin_name}.zip"));
    std::fs::write(&archive_path, &buf).unwrap();
    let mut hasher = Sha256::new();
    hasher.update(&buf);
    let checksum = hex::encode(hasher.finalize());
    (archive_path, checksum)
}

/// A minimal package that declares nothing.
#[cfg(feature = "plugin-packages")]
pub fn build_demo_plugin_zip(dir: &std::path::Path, plugin_name: &str) -> (PathBuf, String) {
    build_plugin_zip(
        dir,
        plugin_name,
        &format!(
            "[plugin]\nname = \"{plugin_name}\"\nversion = \"1.0.0\"\napi_version = \"0.1\"\ndescription = \"demo plugin\"\n"
        ),
    )
}

/// Always-allow permission for tests.
pub struct AllowAllPermission;

#[async_trait::async_trait]
impl base::interface::permission::PermissionPolicy for AllowAllPermission {
    async fn decide(
        &self,
        _req: &base::interface::permission::PermissionRequest<'_>,
    ) -> base::interface::permission::PermissionOutcome {
        base::interface::permission::PermissionOutcome::Permit {
            basis: base::interface::permission::Basis::Bypass,
            profile: Default::default(),
            obligations: Default::default(),
        }
    }
}

/// Bind a test server and return (server, socket_path, _tempdir, join_handle).
pub async fn start_server() -> (
    Arc<DaemonServer>,
    PathBuf,
    tempfile::TempDir,
    tokio::task::JoinHandle<()>,
) {
    start_server_in(tempfile::tempdir().unwrap()).await
}

/// Same server, but rooted at a directory the caller prepared — so a test
/// can install a plugin before the daemon discovers it.
pub async fn start_server_in(
    dir: tempfile::TempDir,
) -> (
    Arc<DaemonServer>,
    PathBuf,
    tempfile::TempDir,
    tokio::task::JoinHandle<()>,
) {
    start_server_serving(
        dir,
        Arc::new(scene::code::spec(scene::code::Interaction::Interactive)),
    )
    .await
}

/// Same server, serving `scene` instead of the interactive code scene.
pub async fn start_server_serving(
    dir: tempfile::TempDir,
    scene: Arc<base::interface::scene::SceneSpec>,
) -> (
    Arc<DaemonServer>,
    PathBuf,
    tempfile::TempDir,
    tokio::task::JoinHandle<()>,
) {
    let sock = dir.path().join("test.sock");
    crate::support::trust_projects_under(dir.path(), dir.path());

    // 使用 dummy client（不真正调 LLM）
    let client: Arc<dyn AnthropicClient> =
        Arc::new(HttpAnthropicClient::new(AuthMode::ApiKey("test-key".into())).unwrap());

    let config = load_daemon_config(
        "claude-sonnet-4-6",
        2000,
        Some(&sock),
        "code",
        &StaticDaemonPaths::new(dir.path().to_path_buf()),
    );

    // Through `daemon::assemble` for the same reason `main.rs` is — including
    // the startup steps a test would otherwise have to remember one at a
    // time, which is how "no session here has any plugin tool" hid once
    // before. `Nowhere` keeps this file's long-standing shape: these cases
    // are about the RPC surface, not about transcripts.
    let pool = daemon::assemble::pool(
        &config,
        scene,
        daemon::Assembly {
            cwd: Some(dir.path().to_path_buf()),
            model_client: Some(client),
            transcripts: daemon::Transcripts::Nowhere,
            host_policy: Some(Arc::new(AllowAllPermission)),
            ..Default::default()
        },
    )
    .await
    .expect("the daemon assembles");

    let cancel = CancellationToken::new();
    let server = Arc::new(DaemonServer::new(pool, cancel));
    let server2 = server.clone();
    let sock2 = sock.clone();
    let handle = tokio::spawn(async move {
        let _ = server2.serve_unix(&sock2).await;
    });

    for _ in 0..20 {
        if sock.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(sock.exists(), "socket never bound");
    (server, sock, dir, handle)
}

pub async fn rpc_call(sock: &std::path::Path, msg: &str) -> String {
    let mut client = UnixStream::connect(sock).await.unwrap();
    client.write_all(msg.as_bytes()).await.unwrap();
    client.write_all(b"\n").await.unwrap();
    let (r, _) = client.split();
    let mut br = BufReader::new(r);
    // A turn streams `session.event` frames ahead of its response.
    loop {
        let mut buf = String::new();
        br.read_line(&mut buf).await.unwrap();
        let is_frame = serde_json::from_str::<serde_json::Value>(&buf)
            .is_ok_and(|v| v["method"] == "session.event");
        if !is_frame {
            return buf;
        }
    }
}

// A malformed session id reaches `SessionPool::resume_or_create`'s parsing
// only when a real `HistoryStore` is configured; `start_server()` uses
// `history_store: None`, so these tests need their own server variant.

pub async fn start_server_with_history(
    cap: usize,
) -> (
    Arc<DaemonServer>,
    PathBuf,
    tempfile::TempDir,
    tokio::task::JoinHandle<()>,
) {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("test.sock");

    let client: Arc<dyn AnthropicClient> =
        Arc::new(HttpAnthropicClient::new(AuthMode::ApiKey("test-key".into())).unwrap());

    let store: Arc<dyn history::store::HistoryStore> = Arc::new(
        history::store::JsonlHistoryStore::with_roots(
            dir.path(),
            history::path::HistoryRoots::under(dir.path()),
        )
        .await
        .unwrap(),
    );

    let mut config = load_daemon_config(
        "claude-sonnet-4-6",
        2000,
        None,
        "code",
        &StaticDaemonPaths::new(dir.path().to_path_buf()),
    );
    config.session_cap = cap;

    let pool = daemon::assemble::pool(
        &config,
        Arc::new(scene::code::spec(scene::code::Interaction::Interactive)),
        daemon::Assembly {
            cwd: Some(dir.path().to_path_buf()),
            model_client: Some(client),
            transcripts: daemon::Transcripts::In(store),
            host_policy: Some(Arc::new(AllowAllPermission)),
            ..Default::default()
        },
    )
    .await
    .expect("the daemon assembles");

    let cancel = CancellationToken::new();
    let server = Arc::new(DaemonServer::new(pool, cancel));
    let server2 = server.clone();
    let sock2 = sock.clone();
    let handle = tokio::spawn(async move {
        let _ = server2.serve_unix(&sock2).await;
    });

    for _ in 0..20 {
        if sock.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(sock.exists(), "socket never bound");
    (server, sock, dir, handle)
}

/// Allow `tools` (rule patterns such as `Bash` or `Bash(touch:*)`) the way a
/// settings file would, for cases that run tools but are not about the gate.
pub fn allow_tools(config: &mut daemon::config::DaemonConfig, tools: &[&str]) {
    config.settings.rules.extend(tools.iter().map(|tool| {
        base::permission::LoadedRule::new(
            base::permission::PermissionRuleEntry {
                tool: (*tool).into(),
                action: base::permission::RuleBehavior::Allow,
                matching: Default::default(),
            },
            base::interface::config_source::Tier::Project,
            false,
        )
    }));
}
