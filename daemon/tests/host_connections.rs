//! A connection can provide tools and hooks to the whole daemon, not only to
//! the sessions it created: `daemon.setHostTools` puts tools in front of the
//! model in every session built afterwards, with the calls going to the
//! provider whether or not it watches the session, and `daemon.setHooks`
//! lets it veto or annotate any session's tool calls. Also covered: progress
//! a host reports for a call it is running, and what happens when the
//! provider leaves, the turn is interrupted, or a hook call goes unanswered.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base::interface::scene::{SceneSpec, ToolSurface};
use daemon::config::{load_daemon_config, StaticDaemonPaths};
use daemon::{DaemonServer, SessionPool};
use model::mock::MockAnthropicClient;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::UnixStream;
use tokio_util::sync::CancellationToken;

use crate::support::{text_round, tool_round};

const INVALID_PARAMS: i64 = -32602;
const LEAN_SCENE: &str = "lean";

fn x_schema() -> Value {
    json!({
        "type": "object",
        "properties": { "text": { "type": "string" } },
        "required": ["text"],
    })
}

fn tool(name: &str, extra: Value) -> Value {
    let mut tool = json!({
        "name": name,
        "description": "Echo text back from the host.",
        "input_schema": x_schema(),
        "read_only": true,
    });
    for (k, v) in extra.as_object().unwrap() {
        tool[k] = v.clone();
    }
    tool
}

fn host_x(extra: Value) -> Value {
    tool("HostX", extra)
}

#[derive(Default)]
struct Setup {
    settings: Value,
    skill: Option<&'static str>,
    /// The tools of a `lean` scene to add, when one is added.
    lean_scene: Option<&'static [&'static str]>,
    git: bool,
}

struct Daemon {
    sock: PathBuf,
    work: PathBuf,
    pool: Arc<SessionPool>,
    model: Arc<MockAnthropicClient>,
    _dir: tempfile::TempDir,
}

struct Conn {
    write: tokio::sync::Mutex<OwnedWriteHalf>,
    frames: Arc<Mutex<Vec<Value>>>,
    responses: Arc<Mutex<HashMap<u64, Value>>>,
    next: std::sync::atomic::AtomicU64,
}

impl Conn {
    async fn send(&self, method: &str, params: Value) -> u64 {
        let id = self.next.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let request = json!({"jsonrpc":"2.0","method":method,"params":params,"id":id});
        self.write
            .lock()
            .await
            .write_all(format!("{request}\n").as_bytes())
            .await
            .unwrap();
        id
    }

    async fn response(&self, id: u64) -> Value {
        for _ in 0..1200 {
            if let Some(r) = self.responses.lock().unwrap().remove(&id) {
                return r;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("no response to request {id}");
    }

    async fn call(&self, method: &str, params: Value) -> Value {
        let id = self.send(method, params).await;
        self.response(id).await
    }

    async fn ok(&self, method: &str, params: Value) -> Value {
        let response = self.call(method, params).await;
        assert!(
            response.get("result").is_some(),
            "{method} failed: {response}"
        );
        response["result"].clone()
    }

    async fn refused(&self, method: &str, params: Value) {
        let response = self.call(method, params).await;
        assert_eq!(
            response["error"]["code"], INVALID_PARAMS,
            "{method} should be refused: {response}"
        );
    }

    async fn leave(&self) {
        self.write.lock().await.shutdown().await.unwrap();
    }

    fn events(&self) -> Vec<Value> {
        self.frames
            .lock()
            .unwrap()
            .iter()
            .map(|f| f["event"].clone())
            .collect()
    }

    async fn event(&self, what: &str, pred: impl Fn(&Value) -> bool) -> Value {
        for _ in 0..1200 {
            if let Some(e) = self.events().into_iter().find(|e| pred(e)) {
                return e;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("no {what}: {:#?}", self.events());
    }

    async fn kind(&self, kind: &str) -> Value {
        self.event(kind, |e| e["kind"] == kind).await
    }

    async fn resolved(&self, reason: &str) -> Value {
        self.event(reason, |e| {
            e["kind"] == "host_tool_call_resolved" && e["reason"] == reason
        })
        .await
    }

    async fn hook_resolved(&self, reason: &str) -> Value {
        self.event(reason, |e| {
            e["kind"] == "hook_call_resolved" && e["reason"] == reason
        })
        .await
    }

    async fn provide(&self, tools: Vec<Value>) -> Value {
        self.ok("daemon.setHostTools", json!({ "host_tools": tools }))
            .await
    }

    async fn hook(&self, entries: Vec<Value>) -> Value {
        self.ok("daemon.setHooks", json!({ "hooks": entries }))
            .await
    }

    async fn answer_tool(&self, session: &str, call: &Value, text: &str) {
        self.ok(
            "session.respondToolCall",
            json!({"session_id": session, "call_id": call["call_id"],
                   "result": {"content": text}}),
        )
        .await;
    }

    async fn answer_hook(&self, session: &str, call: &Value, answer: Value) {
        let mut params = json!({"session_id": session, "call_id": call["call_id"]});
        for (k, v) in answer.as_object().unwrap() {
            params[k] = v.clone();
        }
        self.ok("session.respondHook", params).await;
    }
}

fn lean_scene(tools: &[&str]) -> SceneSpec {
    let mini = scene::mini::spec();
    let mut s = SceneSpec::new(LEAN_SCENE, "Lean", mini.prompt);
    s.tools = ToolSurface::allowing(tools);
    s.token_budget = mini.token_budget;
    s
}

impl Daemon {
    async fn start() -> Self {
        Self::start_with(Setup::default()).await
    }

    async fn start_with(setup: Setup) -> Self {
        let model = Arc::new(MockAnthropicClient::new());
        let dir = tempfile::tempdir().unwrap();
        let dir_path = dir.path().canonicalize().unwrap();
        let sock = dir_path.join("host.sock");
        let work = dir_path.join("work");
        let global = dir_path.join("agix");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::create_dir_all(&global).unwrap();
        if setup.git {
            git_repo(&work);
            crate::support::trust_projects_under(&global, &work);
        }

        let mut settings = json!({
            "sandbox": {"dangerously_disable_sandbox": true},
            "permissions": {"allow": ["HostX", "HostEcho", "Agent", "Bash", "EnterWorktree"]},
        });
        if let Some(extra) = setup.settings.as_object() {
            for (k, v) in extra {
                settings[k] = v.clone();
            }
        }
        std::fs::write(
            global.join("settings.json"),
            settings
                .to_string()
                .replace("{work}", &work.display().to_string()),
        )
        .unwrap();
        if let Some(body) = setup.skill {
            let skill = global.join("skills").join("probe");
            std::fs::create_dir_all(&skill).unwrap();
            std::fs::write(
                skill.join("SKILL.md"),
                format!(
                    "---\nname: probe\ndescription: probes\n---\n{}\n",
                    body.replace("{work}", &work.display().to_string())
                ),
            )
            .unwrap();
        }

        let paths =
            StaticDaemonPaths::with_project(global.clone(), work.clone()).with_global(global);
        let mut config = load_daemon_config("claude-sonnet-4-6", 2000, Some(&sock), "code", &paths);
        config.settings.values.memory_enabled = false;
        let mut assembly = daemon::Assembly {
            cwd: Some(work.clone()),
            model_client: Some(model.clone()),
            ..Default::default()
        };
        if let Some(tools) = setup.lean_scene {
            assembly.scenes = vec![Arc::new(lean_scene(tools))];
            assembly.extra_scenes = vec![LEAN_SCENE.into()];
        }
        let pool = daemon::assemble::pool(
            &config,
            Arc::new(scene::code::spec(scene::code::Interaction::Interactive)),
            assembly,
        )
        .await
        .expect("the daemon assembles");
        let server = Arc::new(DaemonServer::new(pool.clone(), CancellationToken::new()));
        let bind = sock.clone();
        tokio::spawn(async move {
            let _ = server.serve_unix(&bind).await;
        });
        for _ in 0..80 {
            if sock.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(sock.exists(), "socket never bound");
        Self {
            sock,
            work,
            pool,
            model,
            _dir: dir,
        }
    }

    async fn connect(&self) -> Arc<Conn> {
        let (read, write) = UnixStream::connect(&self.sock).await.unwrap().into_split();
        let conn = Arc::new(Conn {
            write: tokio::sync::Mutex::new(write),
            frames: Arc::default(),
            responses: Arc::default(),
            next: 1.into(),
        });
        let (frames, responses) = (conn.frames.clone(), conn.responses.clone());
        tokio::spawn(async move {
            let mut reader = BufReader::new(read);
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                    return;
                }
                let frame: Value = serde_json::from_str(&line).unwrap();
                if let Some(id) = frame.get("id").and_then(Value::as_u64) {
                    responses.lock().unwrap().insert(id, frame);
                } else if frame["method"] == "session.event" {
                    frames.lock().unwrap().push(frame["params"].clone());
                }
            }
        });
        conn
    }

    async fn session(&self, conn: &Conn, options: Value) -> String {
        conn.ok("session.create", json!({ "options": options }))
            .await["session_id"]
            .as_str()
            .unwrap()
            .to_string()
    }

    async fn bypass_session(&self, conn: &Conn) -> String {
        self.session(conn, json!({"permission_mode": "bypassPermissions"}))
            .await
    }

    fn marker(&self) -> PathBuf {
        self.work.join("marker")
    }

    fn touch_marker(&self) -> Value {
        json!({"command": format!("touch {}", self.marker().display())})
    }

    fn request(&self, n: usize) -> Value {
        serde_json::to_value(self.model.nth_request(n).expect("the model was called")).unwrap()
    }

    fn later_requests(&self) -> String {
        (1..self.model.calls())
            .map(|n| self.request(n).to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    async fn calls_reach(&self, n: usize) {
        for _ in 0..1200 {
            if self.model.calls() >= n {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("the model was called {} times, not {n}", self.model.calls());
    }

    fn answer_to(&self, tool_use_id: &str) -> (String, bool) {
        for n in 0..self.model.calls() {
            for message in self.request(n)["messages"].as_array().unwrap() {
                for block in message["content"].as_array().into_iter().flatten() {
                    if block["type"] == "tool_result" && block["tool_use_id"] == tool_use_id {
                        let text = match &block["content"] {
                            Value::String(s) => s.clone(),
                            blocks => blocks
                                .as_array()
                                .unwrap()
                                .iter()
                                .filter_map(|b| b["text"].as_str())
                                .collect(),
                        };
                        return (text, block["is_error"] == true);
                    }
                }
            }
        }
        panic!("the model was never answered for {tool_use_id}");
    }

    async fn run(&self, conn: &Conn, session: &str, message: &str) -> u64 {
        conn.send(
            "session.run_turn",
            json!({"session_id": session, "message": message}),
        )
        .await
    }
}

fn git_repo(dir: &std::path::Path) {
    let script = "git init -q . && git -c user.name=t -c user.email=t@t \
                  commit -q --allow-empty -m init";
    let out = std::process::Command::new("sh")
        .args(["-c", script])
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
}

fn offers(request: &Value, name: &str) -> bool {
    request["tools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|t| t["name"] == name)
}

async fn settled(conn: &Conn) {
    conn.ok("daemon.ping", json!({})).await;
}

fn at(rfc3339: &str) -> time::OffsetDateTime {
    time::OffsetDateTime::parse(rfc3339, &time::format_description::well_known::Rfc3339).unwrap()
}

#[tokio::test]
async fn a_host_reports_progress_for_the_call_it_is_running() {
    let daemon = Daemon::start().await;
    daemon
        .model
        .push_turn(tool_round("HostEcho", "toolu_echo", json!({ "text": "x" })));
    daemon.model.push_turn(text_round("done"));
    let conn = daemon.connect().await;
    let session = daemon
        .session(&conn, json!({"host_tools": [tool("HostEcho", json!({}))]}))
        .await;
    let turn = daemon.run(&conn, &session, "go").await;
    let call = conn.kind("host_tool_call").await;

    for (message, progress) in [("one", 1), ("two", 2)] {
        conn.ok(
            "session.reportToolProgress",
            json!({"session_id": session, "call_id": call["call_id"],
                   "message": message, "progress": progress, "total": 2}),
        )
        .await;
    }
    for (message, progress) in [("one", 1), ("two", 2)] {
        let frame = conn
            .event(message, |e| {
                e["kind"] == "tool_progress"
                    && e["tool_use_id"] == "toolu_echo"
                    && e["message"] == message
            })
            .await;
        assert_eq!(frame["progress"], progress, "{frame}");
        assert_eq!(frame["total"], 2, "{frame}");
    }

    conn.answer_tool(&session, &call, "echoed").await;
    conn.response(turn).await;
    conn.ok(
        "session.reportToolProgress",
        json!({"session_id": session, "call_id": call["call_id"], "message": "late"}),
    )
    .await;
}

#[tokio::test]
async fn a_connections_tool_reaches_it_though_it_does_not_watch_the_session() {
    let daemon = Daemon::start().await;
    daemon.model.push_turn(tool_round(
        "HostX",
        "toolu_x",
        json!({ "text": "ask acme" }),
    ));
    daemon.model.push_turn(text_round("done"));
    let provider = daemon.connect().await;
    let client = daemon.connect().await;
    let set = provider.provide(vec![host_x(json!({}))]).await;
    assert_eq!(set["host_tools"], json!(["HostX"]), "{set}");
    assert_eq!(set["refused"], json!([]), "{set}");

    let session = daemon
        .session(&client, json!({"permission_mode": "default"}))
        .await;
    let turn = daemon.run(&client, &session, "go").await;
    let call = provider.kind("host_tool_call").await;
    assert_eq!(call["name"], "HostX");
    assert_eq!(call["tool_use_id"], "toolu_x");
    assert_eq!(call["input"], json!({"text": "ask acme"}));
    assert_eq!(call["cwd"], daemon.work.display().to_string(), "{call}");
    assert_eq!(call["depth"], 0, "{call}");
    let seen_by_client = client.kind("host_tool_call").await;
    assert_eq!(seen_by_client["call_id"], call["call_id"]);

    client
        .refused(
            "session.respondToolCall",
            json!({"session_id": session, "call_id": call["call_id"],
                   "result": {"content": "from the wrong connection"}}),
        )
        .await;
    client
        .refused(
            "session.reportToolProgress",
            json!({"session_id": session, "call_id": call["call_id"], "message": "x"}),
        )
        .await;
    provider
        .ok(
            "session.reportToolProgress",
            json!({"session_id": session, "call_id": call["call_id"], "message": "working"}),
        )
        .await;
    client
        .event("progress", |e| {
            e["kind"] == "tool_progress" && e["message"] == "working"
        })
        .await;

    provider
        .answer_tool(&session, &call, "answer-from-acme")
        .await;
    client.response(turn).await;
    assert_eq!(
        daemon.answer_to("toolu_x"),
        ("answer-from-acme".to_string(), false)
    );
    assert!(offers(&daemon.request(0), "HostX"));
}

#[tokio::test]
async fn an_entry_with_an_empty_name_is_refused_alone() {
    let daemon = Daemon::start().await;
    let provider = daemon.connect().await;
    let set = provider
        .provide(vec![host_x(json!({"name": ""})), tool("HostOk", json!({}))])
        .await;
    assert_eq!(set["host_tools"], json!(["HostOk"]), "{set}");
    let refused = set["refused"].as_array().unwrap();
    assert_eq!(refused.len(), 1, "{set}");
    assert_eq!(refused[0]["index"], 0);
    assert_eq!(refused[0]["field"], "name");
}

#[tokio::test]
async fn a_session_resumed_after_the_tools_were_set_gets_them() {
    let daemon = Daemon::start().await;
    daemon.model.push_turn(text_round("first"));
    daemon
        .model
        .push_turn(tool_round("HostX", "toolu_x", json!({ "text": "after" })));
    daemon.model.push_turn(text_round("done"));
    let provider = daemon.connect().await;
    let client = daemon.connect().await;
    let session = daemon.session(&client, json!({})).await;
    client
        .ok(
            "session.run_turn",
            json!({"session_id": session, "message": "hello"}),
        )
        .await;
    client
        .ok("session.close", json!({"session_id": session}))
        .await;

    provider.provide(vec![host_x(json!({}))]).await;
    let resumed = client
        .ok("session.resume", json!({"session_id": session}))
        .await;
    assert_eq!(resumed["status"], "resumed", "{resumed}");
    let turn = daemon.run(&client, &session, "now").await;
    let call = provider.kind("host_tool_call").await;
    provider
        .answer_tool(&session, &call, "resumed-answer")
        .await;
    client.response(turn).await;

    assert!(offers(&daemon.request(1), "HostX"));
    assert_eq!(
        daemon.answer_to("toolu_x"),
        ("resumed-answer".to_string(), false)
    );
}

#[tokio::test]
async fn a_sub_agent_calls_a_connections_tool_when_it_is_declared_for_sub_agents() {
    let daemon = Daemon::start().await;
    daemon.model.push_turn(tool_round(
        "Agent",
        "toolu_agent",
        json!({
            "description": "ask the host",
            "prompt": "Call HostX",
            "subagent_type": "general-purpose",
        }),
    ));
    daemon.model.push_turn(tool_round(
        "HostX",
        "toolu_inner",
        json!({ "text": "from below" }),
    ));
    daemon.model.push_turn(text_round("the host said hi"));
    daemon.model.push_turn(text_round("done"));
    let provider = daemon.connect().await;
    let client = daemon.connect().await;
    provider
        .provide(vec![host_x(json!({"subagents": true}))])
        .await;
    let session = daemon.session(&client, json!({})).await;
    let turn = daemon.run(&client, &session, "go").await;

    let call = provider.kind("host_tool_call").await;
    assert_eq!(call["tool_use_id"], "toolu_inner");
    assert_eq!(call["task_id"], "toolu_agent", "{call}");
    let sub_session = call["agent_session_id"].as_str().expect("a sub-agent call");
    assert_ne!(sub_session, session);
    assert_eq!(call["cwd"], daemon.work.display().to_string(), "{call}");
    assert_eq!(call["depth"], 1, "{call}");
    provider.answer_tool(&session, &call, "hi from acme").await;
    client.response(turn).await;

    assert_eq!(
        daemon.answer_to("toolu_inner"),
        ("hi from acme".to_string(), false)
    );
    assert!(offers(&daemon.request(1), "HostX"));
}

#[tokio::test]
async fn a_sub_agent_in_a_worktree_calls_and_is_hooked_from_the_worktree() {
    let daemon = Daemon::start_with(Setup {
        git: true,
        ..Setup::default()
    })
    .await;
    daemon.model.push_turn(tool_round(
        "Agent",
        "toolu_agent",
        json!({
            "description": "work apart",
            "prompt": "Call HostX",
            "subagent_type": "general-purpose",
            "worktree": "probe",
        }),
    ));
    daemon.model.push_turn(tool_round(
        "HostX",
        "toolu_inner",
        json!({ "text": "where am I" }),
    ));
    daemon.model.push_turn(text_round("sub-agent over"));
    daemon.model.push_turn(text_round("done"));
    let provider = daemon.connect().await;
    let client = daemon.connect().await;
    provider
        .provide(vec![host_x(json!({"subagents": true}))])
        .await;
    provider
        .hook(vec![json!({"events": ["PreToolUse"], "tools": ["HostX"]})])
        .await;
    let session = daemon.session(&client, json!({})).await;
    let turn = daemon.run(&client, &session, "go").await;
    let worktree = daemon.work.join(".wkts/probe").display().to_string();

    let hook = provider.kind("hook_call").await;
    assert_eq!(hook["cwd"], worktree, "{hook}");
    assert_eq!(hook["depth"], 1, "{hook}");
    assert_eq!(hook["input"]["cwd"], worktree, "{hook}");
    assert_eq!(hook["input"]["depth"], 1, "{hook}");
    provider
        .answer_hook(&session, &hook, json!({"decision": "proceed"}))
        .await;

    let call = provider.kind("host_tool_call").await;
    assert_eq!(call["cwd"], worktree, "{call}");
    assert_eq!(call["depth"], 1, "{call}");
    provider
        .answer_tool(&session, &call, "in the worktree")
        .await;
    client.response(turn).await;
    assert_eq!(
        daemon.answer_to("toolu_inner"),
        ("in the worktree".to_string(), false)
    );
}

/// Under the sandbox: a sub-agent working in a worktree starts another in a
/// worktree of the project, not one nested in its own, and that one commits
/// in it; the one with the commit stays, the untouched one goes.
#[tokio::test]
async fn a_worktree_sub_agent_starts_its_own_in_a_worktree_of_the_project() {
    let daemon = Daemon::start_with(Setup {
        git: true,
        settings: json!({"sandbox": {"dangerously_disable_sandbox": false}}),
        ..Setup::default()
    })
    .await;
    for (id, slug, prompt) in [
        ("toolu_outer", "outer", "Delegate again"),
        ("toolu_middle", "inner", "Commit, then call HostX"),
    ] {
        daemon.model.push_turn(tool_round(
            "Agent",
            id,
            json!({
                "description": "work apart",
                "prompt": prompt,
                "subagent_type": "general-purpose",
                "worktree": slug,
            }),
        ));
    }
    daemon.model.push_turn(tool_round(
        "Bash",
        "toolu_commit",
        json!({
            "command": "echo f > f && git add f && git -c user.name=t -c user.email=t@t \
                        commit -qm in-the-inner-worktree && git log -1 --format=%s",
        }),
    ));
    daemon.model.push_turn(tool_round(
        "HostX",
        "toolu_inner",
        json!({ "text": "where am I" }),
    ));
    daemon.model.push_turn(text_round("inner over"));
    daemon.model.push_turn(text_round("outer over"));
    daemon.model.push_turn(text_round("done"));
    let provider = daemon.connect().await;
    let client = daemon.connect().await;
    provider
        .provide(vec![host_x(json!({"subagents": true}))])
        .await;
    let session = daemon.session(&client, json!({})).await;
    let turn = daemon.run(&client, &session, "go").await;

    let call = provider.kind("host_tool_call").await;
    let inner = daemon.work.join(".wkts/inner");
    assert_eq!(call["cwd"], inner.display().to_string(), "{call}");
    assert_eq!(call["depth"], 2, "{call}");
    provider.answer_tool(&session, &call, "nested").await;
    client.response(turn).await;
    let (committed, failed) = daemon.answer_to("toolu_commit");
    assert!(
        !failed && committed.contains("in-the-inner-worktree"),
        "{committed}"
    );
    assert_eq!(
        daemon.answer_to("toolu_inner"),
        ("nested".to_string(), false)
    );
    assert!(
        inner.join("f").is_file(),
        "the inner worktree holds a commit"
    );
    assert!(
        !daemon.work.join(".wkts/outer").exists(),
        "the outer worktree was left as it was made"
    );
}

#[tokio::test]
async fn a_call_two_delegations_down_has_depth_two() {
    let daemon = Daemon::start().await;
    for (id, prompt) in [
        ("toolu_outer", "Delegate again"),
        ("toolu_middle", "Call HostX"),
    ] {
        daemon.model.push_turn(tool_round(
            "Agent",
            id,
            json!({
                "description": "go down",
                "prompt": prompt,
                "subagent_type": "general-purpose",
            }),
        ));
    }
    daemon.model.push_turn(tool_round(
        "HostX",
        "toolu_inner",
        json!({ "text": "how deep" }),
    ));
    daemon.model.push_turn(text_round("bottom over"));
    daemon.model.push_turn(text_round("middle over"));
    daemon.model.push_turn(text_round("done"));
    let provider = daemon.connect().await;
    let client = daemon.connect().await;
    provider
        .provide(vec![host_x(json!({"subagents": true}))])
        .await;
    let session = daemon.session(&client, json!({})).await;
    let turn = daemon.run(&client, &session, "go").await;

    let call = provider.kind("host_tool_call").await;
    assert_eq!(call["depth"], 2, "{call}");
    assert_eq!(call["cwd"], daemon.work.display().to_string(), "{call}");
    provider.answer_tool(&session, &call, "two down").await;
    client.response(turn).await;
    assert_eq!(
        daemon.answer_to("toolu_inner"),
        ("two down".to_string(), false)
    );
}

#[tokio::test]
async fn entering_a_worktree_leaves_the_sessions_cwd_where_it_was() {
    let daemon = Daemon::start_with(Setup {
        git: true,
        lean_scene: Some(&["Read", "EnterWorktree"]),
        ..Setup::default()
    })
    .await;
    daemon.model.push_turn(tool_round(
        "EnterWorktree",
        "toolu_enter",
        json!({ "slug": "probe" }),
    ));
    daemon.model.push_turn(tool_round(
        "HostX",
        "toolu_x",
        json!({ "text": "where am I" }),
    ));
    daemon.model.push_turn(text_round("done"));
    let provider = daemon.connect().await;
    let client = daemon.connect().await;
    provider.provide(vec![host_x(json!({}))]).await;
    provider
        .hook(vec![json!({"events": ["PreToolUse"], "tools": ["HostX"]})])
        .await;
    let session = client
        .ok("session.create", json!({"scene": LEAN_SCENE}))
        .await["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    let turn = daemon.run(&client, &session, "go").await;
    let work = daemon.work.display().to_string();

    let hook = provider.kind("hook_call").await;
    assert_eq!(
        (&hook["cwd"], &hook["input"]["cwd"]),
        (&json!(work), &json!(work))
    );
    assert_eq!(hook["depth"], 0, "{hook}");
    provider
        .answer_hook(&session, &hook, json!({"decision": "proceed"}))
        .await;
    let call = provider.kind("host_tool_call").await;
    assert_eq!(call["cwd"], work, "{call}");
    provider.answer_tool(&session, &call, "still home").await;
    client.response(turn).await;
    let (entered, failed) = daemon.answer_to("toolu_enter");
    assert!(!failed, "{entered}");
    assert!(daemon.work.join(".wkts/probe").is_dir());
}

#[tokio::test]
async fn a_scene_with_a_tool_whitelist_still_gets_a_connections_tools() {
    let daemon = Daemon::start_with(Setup {
        lean_scene: Some(&["Read"]),
        ..Setup::default()
    })
    .await;
    daemon
        .model
        .push_turn(tool_round("HostX", "toolu_x", json!({ "text": "lean" })));
    daemon.model.push_turn(text_round("done"));
    let provider = daemon.connect().await;
    let client = daemon.connect().await;
    provider.provide(vec![host_x(json!({}))]).await;
    let created = client
        .ok("session.create", json!({"scene": LEAN_SCENE}))
        .await;
    assert_eq!(created["scene"], LEAN_SCENE, "{created}");
    let session = created["session_id"].as_str().unwrap().to_string();
    let turn = daemon.run(&client, &session, "go").await;

    daemon.calls_reach(1).await;
    let first = daemon.request(0);
    assert!(offers(&first, "HostX"), "{first}");
    assert!(!offers(&first, "Bash"), "the whitelist still applies");
    let call = provider.kind("host_tool_call").await;
    provider.answer_tool(&session, &call, "lean-answer").await;
    client.response(turn).await;
    assert_eq!(
        daemon.answer_to("toolu_x"),
        ("lean-answer".to_string(), false)
    );
}

#[tokio::test]
async fn a_scheduled_job_calls_a_connections_tool_with_nobody_watching() {
    let daemon = Daemon::start().await;
    daemon
        .model
        .push_turn(tool_round("HostX", "toolu_x", json!({ "text": "nine" })));
    daemon.model.push_turn(text_round("reported"));
    let provider = daemon.connect().await;
    let client = daemon.connect().await;
    provider.provide(vec![host_x(json!({}))]).await;
    let session = daemon.session(&client, json!({})).await;
    client
        .ok(
            "schedule.create",
            json!({"session_id": session, "cron": "0 9 * * *", "prompt": "the nine o'clock report"}),
        )
        .await;
    client
        .ok("session.close", json!({"session_id": session}))
        .await;

    let pool = daemon.pool.clone();
    let job = tokio::spawn(async move { pool.run_due_jobs(at("2026-10-26T09:00:10Z")).await });
    let call = provider.kind("host_tool_call").await;
    assert_eq!(call["tool_use_id"], "toolu_x");
    provider.answer_tool(&session, &call, "nine-answer").await;
    daemon.calls_reach(2).await;
    job.await.unwrap();

    assert_eq!(
        daemon.answer_to("toolu_x"),
        ("nine-answer".to_string(), false)
    );
}

#[tokio::test]
async fn withdrawn_tools_end_at_once_in_sessions_built_before_and_vanish_from_new_ones() {
    let daemon = Daemon::start().await;
    daemon.model.push_turn(text_round("built"));
    daemon.model.push_turn(text_round("fresh"));
    daemon
        .model
        .push_turn(tool_round("HostX", "toolu_x", json!({ "text": "gone" })));
    daemon.model.push_turn(text_round("noted"));
    let provider = daemon.connect().await;
    let client = daemon.connect().await;
    provider.provide(vec![host_x(json!({}))]).await;
    let old = daemon.session(&client, json!({})).await;
    client
        .ok(
            "session.run_turn",
            json!({"session_id": old, "message": "build it"}),
        )
        .await;
    assert!(offers(&daemon.request(0), "HostX"));

    let set = provider.provide(vec![]).await;
    assert_eq!(set["host_tools"], json!([]), "{set}");
    let fresh = daemon.session(&client, json!({})).await;
    client
        .ok(
            "session.run_turn",
            json!({"session_id": fresh, "message": "hello"}),
        )
        .await;
    assert!(!offers(&daemon.request(1), "HostX"));

    client
        .ok(
            "session.run_turn",
            json!({"session_id": old, "message": "call it"}),
        )
        .await;
    let resolved = client.resolved("no_host").await;
    assert_eq!(resolved["resolution"], "withdrawn", "{resolved}");
    let (text, is_error) = daemon.answer_to("toolu_x");
    assert!(is_error && text.contains("no host"), "{text}");
}

#[tokio::test]
async fn a_provider_that_leaves_fails_its_calls_and_later_ones() {
    let daemon = Daemon::start().await;
    daemon
        .model
        .push_turn(tool_round("HostX", "toolu_one", json!({ "text": "a" })));
    daemon.model.push_turn(text_round("first over"));
    daemon
        .model
        .push_turn(tool_round("HostX", "toolu_two", json!({ "text": "b" })));
    daemon.model.push_turn(text_round("second over"));
    let provider = daemon.connect().await;
    let client = daemon.connect().await;
    provider.provide(vec![host_x(json!({}))]).await;
    let session = daemon.session(&client, json!({})).await;
    let turn = daemon.run(&client, &session, "go").await;
    provider.kind("host_tool_call").await;

    provider.leave().await;
    let resolved = client.resolved("host_gone").await;
    assert_eq!(resolved["resolution"], "withdrawn", "{resolved}");
    let reply = client.response(turn).await;
    assert!(reply["result"].is_object(), "{reply}");
    client.kind("turn_complete").await;

    client
        .ok(
            "session.run_turn",
            json!({"session_id": session, "message": "again"}),
        )
        .await;
    client.resolved("no_host").await;
    let (text, is_error) = daemon.answer_to("toolu_two");
    assert!(is_error && text.contains("no host"), "{text}");
}

#[tokio::test]
async fn a_sessions_own_tool_wins_over_a_connections_of_the_same_name() {
    let daemon = Daemon::start().await;
    daemon
        .model
        .push_turn(tool_round("HostX", "toolu_x", json!({ "text": "mine" })));
    daemon.model.push_turn(text_round("done"));
    let provider = daemon.connect().await;
    let client = daemon.connect().await;
    provider.provide(vec![host_x(json!({}))]).await;
    let session = daemon
        .session(
            &client,
            json!({"host_tools": [host_x(json!({"description": "the session's own"}))]}),
        )
        .await;
    let turn = daemon.run(&client, &session, "go").await;
    let call = client.kind("host_tool_call").await;
    client.answer_tool(&session, &call, "own-answer").await;
    client.response(turn).await;

    let first = daemon.request(0);
    let offered = first["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "HostX")
        .expect("HostX is offered");
    assert_eq!(offered["description"], "the session's own");
    settled(&provider).await;
    assert!(
        !provider
            .events()
            .iter()
            .any(|e| e["kind"] == "host_tool_call"),
        "{:#?}",
        provider.events()
    );
    assert_eq!(
        daemon.answer_to("toolu_x"),
        ("own-answer".to_string(), false)
    );
}

#[tokio::test]
async fn the_first_connection_keeps_a_tool_name() {
    let daemon = Daemon::start().await;
    let first = daemon.connect().await;
    let second = daemon.connect().await;
    let kept = first.provide(vec![host_x(json!({}))]).await;
    assert_eq!(kept["host_tools"], json!(["HostX"]), "{kept}");

    let set = second.provide(vec![host_x(json!({}))]).await;
    assert_eq!(set["host_tools"], json!([]), "{set}");
    let refused = set["refused"].as_array().unwrap();
    assert_eq!(refused.len(), 1, "{set}");
    assert_eq!(refused[0]["index"], 0);
    assert_eq!(refused[0]["name"], "HostX");
    assert_eq!(refused[0]["field"], "name");
    assert!(
        refused[0]["error"]
            .as_str()
            .unwrap()
            .contains("another connection"),
        "{set}"
    );
}

#[tokio::test]
async fn a_bad_entry_is_refused_alone_when_setting_host_tools() {
    let daemon = Daemon::start().await;
    let provider = daemon.connect().await;
    let set = provider
        .provide(vec![
            tool("Read", json!({})),
            tool("HostBad", json!({"input_schema": {"type": "string"}})),
            tool("HostGood", json!({})),
        ])
        .await;
    assert_eq!(set["host_tools"], json!(["HostGood"]), "{set}");
    let refused = set["refused"].as_array().unwrap();
    assert_eq!(refused.len(), 2, "{set}");
    assert_eq!(refused[0]["index"], 0);
    assert_eq!(refused[0]["field"], "name");
    assert_eq!(refused[1]["index"], 1);
    assert_eq!(refused[1]["field"], "input_schema");
}

#[tokio::test]
async fn a_pre_tool_use_hook_from_a_connection_blocks_a_call_nobody_else_hears_of() {
    let daemon = Daemon::start().await;
    daemon
        .model
        .push_turn(tool_round("Bash", "toolu_bash", daemon.touch_marker()));
    daemon.model.push_turn(text_round("blocked"));
    let hooker = daemon.connect().await;
    let client = daemon.connect().await;
    let set = hooker
        .hook(vec![json!({"events": ["PreToolUse"], "tools": ["Bash"]})])
        .await;
    assert_eq!(
        set["hooks"],
        json!([{"index": 0, "events": ["PreToolUse"]}]),
        "{set}"
    );
    assert_eq!(set["refused"], json!([]), "{set}");

    let session = daemon.bypass_session(&client).await;
    let turn = daemon.run(&client, &session, "touch it").await;
    let call = hooker.kind("hook_call").await;
    assert_eq!(call["hook_event"], "PreToolUse");
    assert_eq!(call["input"]["tool_name"], "Bash");
    assert!(
        call["input"]["tool_input"]["command"]
            .as_str()
            .unwrap()
            .contains("marker"),
        "{call}"
    );

    hooker
        .refused(
            "session.respondHook",
            json!({"session_id": session, "call_id": call["call_id"], "decision": "approve"}),
        )
        .await;
    hooker
        .refused(
            "session.respondHook",
            json!({"session_id": session, "call_id": call["call_id"],
                   "decision": "proceed", "updated_input": {"command": "true"}}),
        )
        .await;
    client
        .refused(
            "session.respondHook",
            json!({"session_id": session, "call_id": call["call_id"], "decision": "proceed"}),
        )
        .await;

    hooker
        .answer_hook(
            &session,
            &call,
            json!({"decision": "block", "message": "nope from acme"}),
        )
        .await;
    client.response(turn).await;
    let resolved = hooker.kind("hook_call_resolved").await;
    assert_eq!(resolved["resolution"], "answered", "{resolved}");

    let (text, is_error) = daemon.answer_to("toolu_bash");
    assert!(
        is_error && text.contains("Blocked by PreToolUse hook: nope from acme"),
        "{text}"
    );
    assert!(!daemon.marker().exists());
    assert!(
        !client
            .events()
            .iter()
            .any(|e| e["kind"] == "hook_call" || e["kind"] == "hook_call_resolved"),
        "{:#?}",
        client.events()
    );
}

#[tokio::test]
async fn a_hooks_additional_context_reaches_the_model_and_the_call_goes_on() {
    let daemon = Daemon::start().await;
    daemon
        .model
        .push_turn(tool_round("Bash", "toolu_bash", daemon.touch_marker()));
    daemon.model.push_turn(text_round("done"));
    let hooker = daemon.connect().await;
    let client = daemon.connect().await;
    hooker
        .hook(vec![json!({"events": ["PreToolUse"], "tools": ["Bash"]})])
        .await;
    let session = daemon.bypass_session(&client).await;
    let turn = daemon.run(&client, &session, "touch it").await;
    let call = hooker.kind("hook_call").await;
    hooker
        .answer_hook(
            &session,
            &call,
            json!({"decision": "proceed", "additional_context": "ctx-acme-123"}),
        )
        .await;
    client.response(turn).await;

    assert!(daemon.marker().exists());
    assert!(
        daemon.later_requests().contains("ctx-acme-123"),
        "{}",
        daemon.later_requests()
    );
}

#[tokio::test]
async fn a_sub_agents_tool_call_is_hooked_and_answered_with_the_parents_session() {
    let daemon = Daemon::start().await;
    daemon.model.push_turn(tool_round(
        "Agent",
        "toolu_agent",
        json!({
            "description": "touch",
            "prompt": "Run the command",
            "subagent_type": "general-purpose",
        }),
    ));
    daemon
        .model
        .push_turn(tool_round("Bash", "toolu_inner", daemon.touch_marker()));
    daemon.model.push_turn(text_round("sub-agent over"));
    daemon.model.push_turn(text_round("done"));
    let hooker = daemon.connect().await;
    let client = daemon.connect().await;
    hooker
        .hook(vec![json!({"events": ["PreToolUse"], "tools": ["Bash"]})])
        .await;
    let session = daemon.bypass_session(&client).await;
    let turn = daemon.run(&client, &session, "delegate").await;

    let call = hooker.kind("hook_call").await;
    let sub_session = call["agent_session_id"].as_str().expect("a sub-agent call");
    assert_ne!(sub_session, session);
    assert_eq!(call["input"]["tool_name"], "Bash");
    hooker
        .answer_hook(
            &session,
            &call,
            json!({"decision": "block", "message": "no shell below"}),
        )
        .await;
    client.response(turn).await;

    let (text, is_error) = daemon.answer_to("toolu_inner");
    assert!(
        is_error && text.contains("Blocked by PreToolUse hook: no shell below"),
        "{text}"
    );
    assert!(!daemon.marker().exists());
}

#[cfg(feature = "skills")]
#[tokio::test]
async fn a_skills_inline_command_is_a_bash_call_a_connections_hook_can_block() {
    let daemon = Daemon::start_with(Setup {
        skill: Some("Marker: !`touch {work}/inline-marker`"),
        ..Setup::default()
    })
    .await;
    daemon.model.push_turn(tool_round(
        "Skill",
        "toolu_skill",
        json!({"skill": "probe"}),
    ));
    daemon.model.push_turn(text_round("done"));
    let hooker = daemon.connect().await;
    let client = daemon.connect().await;
    hooker
        .hook(vec![json!({"events": ["PreToolUse"], "tools": ["Bash"]})])
        .await;
    let session = daemon.bypass_session(&client).await;
    let turn = daemon.run(&client, &session, "use the probe skill").await;

    let call = hooker.kind("hook_call").await;
    assert_eq!(call["input"]["tool_name"], "Bash");
    assert!(
        call["input"]["tool_input"]["command"]
            .as_str()
            .unwrap()
            .contains("inline-marker"),
        "{call}"
    );
    hooker
        .answer_hook(
            &session,
            &call,
            json!({"decision": "block", "message": "inline says no"}),
        )
        .await;
    client.response(turn).await;

    assert!(!daemon.work.join("inline-marker").exists());
    assert!(
        daemon.later_requests().contains("inline says no"),
        "{}",
        daemon.later_requests()
    );
}

#[tokio::test]
async fn a_hook_nobody_answers_times_out_and_lets_the_call_go() {
    let daemon = Daemon::start().await;
    daemon
        .model
        .push_turn(tool_round("Bash", "toolu_bash", daemon.touch_marker()));
    daemon.model.push_turn(text_round("done"));
    let hooker = daemon.connect().await;
    let client = daemon.connect().await;
    hooker
        .hook(vec![
            json!({"events": ["PreToolUse"], "tools": ["Bash"], "timeout_ms": 1000}),
        ])
        .await;
    let session = daemon.bypass_session(&client).await;
    let turn = daemon.run(&client, &session, "touch it").await;
    hooker.kind("hook_call").await;

    let resolved = hooker.hook_resolved("timed_out").await;
    assert_eq!(resolved["resolution"], "withdrawn", "{resolved}");
    client.response(turn).await;
    assert!(daemon.marker().exists());
}

#[tokio::test]
async fn a_hook_that_fails_blocks_when_its_on_error_says_so() {
    let daemon = Daemon::start().await;
    daemon
        .model
        .push_turn(tool_round("Bash", "toolu_bash", daemon.touch_marker()));
    daemon.model.push_turn(text_round("done"));
    let hooker = daemon.connect().await;
    let client = daemon.connect().await;
    hooker
        .hook(vec![json!({"events": ["PreToolUse"], "tools": ["Bash"],
                          "timeout_ms": 1000, "on_error": "block"})])
        .await;
    let session = daemon.bypass_session(&client).await;
    let turn = daemon.run(&client, &session, "touch it").await;
    hooker.kind("hook_call").await;

    hooker.hook_resolved("timed_out").await;
    client.response(turn).await;
    let (text, is_error) = daemon.answer_to("toolu_bash");
    assert!(is_error && text.contains("on_error is block"), "{text}");
    assert!(!daemon.marker().exists());
}

#[tokio::test]
async fn interrupting_the_turn_withdraws_a_waiting_hook_call() {
    let daemon = Daemon::start().await;
    daemon
        .model
        .push_turn(tool_round("Bash", "toolu_bash", daemon.touch_marker()));
    daemon.model.push_turn(text_round("next"));
    let hooker = daemon.connect().await;
    let client = daemon.connect().await;
    hooker
        .hook(vec![json!({"events": ["PreToolUse"], "tools": ["Bash"]})])
        .await;
    let session = daemon.bypass_session(&client).await;
    let turn = daemon.run(&client, &session, "touch it").await;
    hooker.kind("hook_call").await;

    client
        .ok("session.interrupt", json!({"session_id": session}))
        .await;
    let resolved = hooker.hook_resolved("cancelled").await;
    assert_eq!(resolved["resolution"], "withdrawn", "{resolved}");
    client.response(turn).await;
    assert!(!daemon.marker().exists());
}

#[tokio::test]
async fn a_hook_provider_that_leaves_lets_its_waiting_call_go_on_at_once() {
    let daemon = Daemon::start().await;
    daemon
        .model
        .push_turn(tool_round("Bash", "toolu_bash", daemon.touch_marker()));
    daemon.model.push_turn(text_round("done"));
    let hooker = daemon.connect().await;
    let client = daemon.connect().await;
    hooker
        .hook(vec![
            json!({"events": ["PreToolUse"], "tools": ["Bash"], "timeout_ms": 30000}),
        ])
        .await;
    let session = daemon.bypass_session(&client).await;
    let turn = daemon.run(&client, &session, "touch it").await;
    hooker.kind("hook_call").await;

    hooker.leave().await;
    let reply = client.response(turn).await;
    assert!(reply["result"].is_object(), "{reply}");
    client.kind("turn_complete").await;
    assert!(daemon.marker().exists());
}

fn blocking_settings_hook() -> Value {
    json!({"hooks_config": {"PreToolUse": [{
        "type": "command",
        "command": r#"echo '{"decision":"block","message":"settings say no"}'"#,
        "if": "Bash",
    }]}})
}

#[tokio::test]
async fn a_settings_hook_that_blocks_spares_the_connections_hook_the_call() {
    let daemon = Daemon::start_with(Setup {
        settings: blocking_settings_hook(),
        ..Setup::default()
    })
    .await;
    daemon
        .model
        .push_turn(tool_round("Bash", "toolu_bash", daemon.touch_marker()));
    daemon.model.push_turn(text_round("done"));
    let hooker = daemon.connect().await;
    let client = daemon.connect().await;
    hooker
        .hook(vec![json!({"events": ["PreToolUse"], "tools": ["Bash"]})])
        .await;
    let session = daemon.bypass_session(&client).await;
    let turn = daemon.run(&client, &session, "touch it").await;
    client.response(turn).await;
    settled(&hooker).await;

    assert!(
        !hooker.events().iter().any(|e| e["kind"] == "hook_call"),
        "{:#?}",
        hooker.events()
    );
    let (text, is_error) = daemon.answer_to("toolu_bash");
    assert!(
        is_error && text.contains("Blocked by PreToolUse hook: settings say no"),
        "{text}"
    );
    assert!(!daemon.marker().exists());
}

#[tokio::test]
async fn a_settings_hook_that_lets_the_call_pass_still_asks_the_connections_hook() {
    let daemon = Daemon::start_with(Setup {
        settings: json!({"hooks_config": {"PreToolUse": [{
            "type": "command",
            "command": "touch {work}/settings-ran",
            "if": "Bash",
        }]}}),
        ..Setup::default()
    })
    .await;
    daemon
        .model
        .push_turn(tool_round("Bash", "toolu_bash", daemon.touch_marker()));
    daemon.model.push_turn(text_round("done"));
    let hooker = daemon.connect().await;
    let client = daemon.connect().await;
    hooker
        .hook(vec![json!({"events": ["PreToolUse"], "tools": ["Bash"]})])
        .await;
    let session = daemon.bypass_session(&client).await;
    let turn = daemon.run(&client, &session, "touch it").await;

    let call = hooker.kind("hook_call").await;
    assert!(daemon.work.join("settings-ran").exists());
    hooker
        .answer_hook(&session, &call, json!({"decision": "proceed"}))
        .await;
    client.response(turn).await;
    assert!(daemon.marker().exists());
}

#[tokio::test]
async fn bad_hook_entries_are_refused_one_by_one_and_the_count_is_capped() {
    let daemon = Daemon::start().await;
    let hooker = daemon.connect().await;
    let set = hooker
        .hook(vec![
            json!({"events": ["SessionStart"]}),
            json!({"events": ["PreToolUse"], "timeout_ms": 500}),
            json!({"events": ["PreToolUse"]}),
        ])
        .await;
    assert_eq!(
        set["hooks"],
        json!([{"index": 2, "events": ["PreToolUse"]}]),
        "{set}"
    );
    let refused = set["refused"].as_array().unwrap();
    assert_eq!(refused.len(), 2, "{set}");
    assert_eq!(refused[0]["index"], 0);
    assert_eq!(refused[0]["field"], "events");
    assert_eq!(refused[1]["index"], 1);
    assert_eq!(refused[1]["field"], "timeout_ms");

    let many = hooker
        .hook((0..17).map(|_| json!({"events": ["Stop"]})).collect())
        .await;
    assert_eq!(many["hooks"].as_array().unwrap().len(), 16, "{many}");
    let refused = many["refused"].as_array().unwrap();
    assert_eq!(refused.len(), 1, "{many}");
    assert_eq!(refused[0]["index"], 16);

    let none = hooker.ok("daemon.setHooks", json!({"hooks": null})).await;
    assert_eq!(none["hooks"], json!([]), "{none}");
}

#[tokio::test]
async fn the_daemon_announces_what_a_connection_can_provide() {
    let daemon = Daemon::start().await;
    let conn = daemon.connect().await;
    let ping = conn.ok("daemon.ping", json!({})).await;
    let capabilities = ping["capabilities"].as_array().unwrap();
    for name in [
        "host_tool_progress",
        "host_tool_provider",
        "host_hooks",
        "host_call_context",
    ] {
        assert!(capabilities.iter().any(|c| c == name), "{name}: {ping}");
    }
}

/// Writes one request and closes at once, without reading the reply: the read
/// loop may see the end of the stream, and drop the connection, before the
/// request is served.
async fn send_and_hang_up(daemon: &Daemon, method: &str, params: Value) {
    let mut raw = UnixStream::connect(&daemon.sock).await.unwrap();
    let request = json!({"jsonrpc":"2.0","method":method,"params":params,"id":1});
    raw.write_all(format!("{request}\n").as_bytes())
        .await
        .unwrap();
    raw.shutdown().await.unwrap();
    drop(raw);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn connections_that_hang_up_while_their_tools_are_being_set_keep_no_name() {
    let daemon = Daemon::start().await;
    let names: Vec<String> = (0..40).map(|i| format!("HostX{i}")).collect();
    for name in &names {
        send_and_hang_up(
            &daemon,
            "daemon.setHostTools",
            json!({"host_tools": [tool(name, json!({}))]}),
        )
        .await;
    }
    tokio::time::sleep(Duration::from_secs(2)).await;

    let next = daemon.connect().await;
    let set = next
        .provide(names.iter().map(|n| tool(n, json!({}))).collect())
        .await;
    assert_eq!(
        set["refused"],
        json!([]),
        "connections that are gone still hold these names: {set}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn connections_that_hang_up_while_their_hooks_are_being_set_block_nobody() {
    let daemon = Daemon::start().await;
    daemon
        .model
        .push_turn(tool_round("Bash", "toolu_bash", daemon.touch_marker()));
    daemon.model.push_turn(text_round("done"));
    for _ in 0..8 {
        send_and_hang_up(
            &daemon,
            "daemon.setHooks",
            json!({"hooks": [{"events": ["PreToolUse"], "tools": ["Bash"],
                              "timeout_ms": 1000, "on_error": "block"}]}),
        )
        .await;
    }
    tokio::time::sleep(Duration::from_secs(2)).await;

    let client = daemon.connect().await;
    let room = client
        .call("daemon.setHooks", json!({"hooks": [{"events": ["Stop"]}]}))
        .await;
    client.ok("daemon.setHooks", json!({"hooks": []})).await;
    let session = daemon.bypass_session(&client).await;
    let turn = daemon.run(&client, &session, "touch it").await;
    client.response(turn).await;
    let (text, _) = daemon.answer_to("toolu_bash");
    assert!(
        daemon.marker().exists(),
        "a hook of a connection that is gone blocked another session's call: {text}"
    );
    assert!(
        room.get("result").is_some(),
        "connections that are gone take up the hook connections' places: {room}"
    );
}

/// A relative deny rule of the project covers the main checkout's file for
/// a sub-agent working in a worktree, whether it names the file by its
/// absolute path, climbs to it, or searches the checkout.
#[tokio::test]
async fn a_relative_deny_covers_the_main_checkout_for_a_worktree_sub_agent() {
    let daemon = Daemon::start_with(Setup {
        git: true,
        settings: json!({
            "permissions": {
                "allow": ["Agent", "Read"],
                "deny": ["Read(.env)"]
            },
        }),
        ..Setup::default()
    })
    .await;
    std::fs::write(daemon.work.join(".env"), "SECRET=hunter2\n").unwrap();
    let secret = daemon.work.join(".env").display().to_string();
    daemon.model.push_turn(tool_round(
        "Agent",
        "toolu_agent",
        json!({
            "description": "work apart",
            "prompt": "Read the env",
            "subagent_type": "general-purpose",
            "worktree": "probe",
        }),
    ));
    for (id, path) in [
        ("toolu_absolute", secret.as_str()),
        ("toolu_climbing", "../../.env"),
    ] {
        daemon
            .model
            .push_turn(tool_round("Read", id, json!({ "file_path": path })));
    }
    daemon.model.push_turn(tool_round(
        "Grep",
        "toolu_search",
        json!({ "pattern": "SECRET", "path": daemon.work, "output_mode": "content" }),
    ));
    daemon.model.push_turn(text_round("sub-agent over"));
    daemon.model.push_turn(text_round("done"));
    let client = daemon.connect().await;
    let session = daemon.session(&client, json!({})).await;
    let turn = daemon.run(&client, &session, "go").await;
    client.response(turn).await;
    for id in ["toolu_absolute", "toolu_climbing"] {
        let (text, is_error) = daemon.answer_to(id);
        assert!(
            is_error && !text.contains("hunter2") && text.contains("Read(.env)"),
            "{id}: the worktree sub-agent read the main checkout's denied .env: {text}"
        );
    }
    let (found, _) = daemon.answer_to("toolu_search");
    assert!(
        !found.contains("hunter2"),
        "the search went through .env: {found}"
    );
}

/// A deny rule anchored at the project, absolute or relative, holds for the
/// copy of a committed file in a worktree: for the sub-agent working there,
/// and for the root session once the worktree is kept.
#[tokio::test]
async fn a_project_deny_covers_the_worktrees_copy_of_a_committed_file() {
    let daemon = Daemon::start_with(Setup {
        git: true,
        settings: json!({
            "permissions": {
                "allow": ["Agent", "Bash", "Read"],
                "deny": ["Read({work}/secrets/**)", "Read(vault/**)"]
            },
        }),
        ..Setup::default()
    })
    .await;
    for (dir, body) in [
        ("secrets", "TOKEN=hunter2\n"),
        ("vault", "PASS=swordfish\n"),
    ] {
        std::fs::create_dir_all(daemon.work.join(dir)).unwrap();
        std::fs::write(daemon.work.join(dir).join("key"), body).unwrap();
    }
    let commit = std::process::Command::new("sh")
        .args([
            "-c",
            "git add secrets vault && git -c user.name=t -c user.email=t@t commit -qm keys",
        ])
        .current_dir(&daemon.work)
        .output()
        .unwrap();
    assert!(commit.status.success(), "{commit:?}");
    daemon.model.push_turn(tool_round(
        "Agent",
        "toolu_agent",
        json!({
            "description": "work apart",
            "prompt": "Read the key",
            "subagent_type": "general-purpose",
            "worktree": "probe",
        }),
    ));
    daemon.model.push_turn(tool_round(
        "Read",
        "toolu_own_copy",
        json!({ "file_path": "secrets/key" }),
    ));
    daemon.model.push_turn(tool_round(
        "Bash",
        "toolu_leave",
        json!({ "command": "echo note > left.txt" }),
    ));
    daemon.model.push_turn(text_round("sub-agent over"));
    daemon.model.push_turn(tool_round(
        "Read",
        "toolu_kept_copy",
        json!({ "file_path": ".wkts/probe/vault/key" }),
    ));
    daemon.model.push_turn(text_round("done"));
    let client = daemon.connect().await;
    let session = daemon.session(&client, json!({})).await;
    let turn = daemon.run(&client, &session, "go").await;
    client.response(turn).await;
    assert!(daemon.work.join(".wkts/probe/vault/key").is_file());
    for (id, secret, rule) in [
        ("toolu_own_copy", "hunter2", "secrets/**"),
        ("toolu_kept_copy", "swordfish", "Read(vault/**)"),
    ] {
        let (text, is_error) = daemon.answer_to(id);
        assert!(
            is_error && !text.contains(secret) && text.contains(rule),
            "{id}: the worktree's copy of a denied file was read: {text}"
        );
    }
}

fn branch_exists(work: &std::path::Path, branch: &str) -> bool {
    let out = std::process::Command::new("git")
        .args(["branch", "--list", branch])
        .current_dir(work)
        .output()
        .unwrap();
    !String::from_utf8_lossy(&out.stdout).trim().is_empty()
}

/// A worktree sub-agent's commit outlives the call: the worktree and its
/// branch stay, and the result says where. One that left nothing behind
/// takes its worktree and branch with it.
#[tokio::test]
async fn a_worktree_with_work_in_it_is_kept_and_named_and_an_idle_one_is_removed() {
    let daemon = Daemon::start_with(Setup {
        git: true,
        ..Setup::default()
    })
    .await;
    daemon.model.push_turn(tool_round(
        "Agent",
        "toolu_working",
        json!({
            "description": "work apart",
            "prompt": "Make the file",
            "subagent_type": "general-purpose",
            "worktree": "probe",
        }),
    ));
    daemon.model.push_turn(tool_round(
        "Bash",
        "toolu_commit",
        json!({ "command": "echo kept > made.txt && git add made.txt && git -c user.name=t -c user.email=t@t commit -qm made" }),
    ));
    daemon.model.push_turn(text_round("committed"));
    daemon.model.push_turn(tool_round(
        "Agent",
        "toolu_idle",
        json!({
            "description": "look around",
            "prompt": "Change nothing",
            "subagent_type": "general-purpose",
            "worktree": "idle",
        }),
    ));
    daemon
        .model
        .push_turn(tool_round("Bash", "toolu_look", json!({ "command": "ls" })));
    daemon.model.push_turn(text_round("nothing to do"));
    daemon.model.push_turn(text_round("done"));
    let client = daemon.connect().await;
    let session = daemon.session(&client, json!({})).await;
    let turn = daemon.run(&client, &session, "go").await;
    client.response(turn).await;
    let (bash, bash_err) = daemon.answer_to("toolu_commit");
    assert!(!bash_err, "{bash}");

    let worktree = daemon.work.join(".wkts/probe");
    assert!(worktree.join("made.txt").is_file());
    assert!(!daemon.work.join("made.txt").exists());
    assert!(branch_exists(&daemon.work, "agix/worktree-probe"));
    let log = std::process::Command::new("git")
        .args(["log", "-1", "--format=%s", "agix/worktree-probe"])
        .current_dir(&daemon.work)
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&log.stdout).trim(), "made");
    let (result, is_error) = daemon.answer_to("toolu_working");
    assert!(!is_error, "{result}");
    assert!(result.starts_with("committed"), "{result}");
    assert!(
        result.contains(&format!("<path>{}</path>", worktree.display()))
            && result.contains("<branch>agix/worktree-probe</branch>")
            && result.contains("1 commit(s) since it was made"),
        "{result}"
    );

    assert!(!daemon.work.join(".wkts/idle").exists());
    assert!(!branch_exists(&daemon.work, "agix/worktree-idle"));
    let (result, _) = daemon.answer_to("toolu_idle");
    assert_eq!(result, "nothing to do");
}

/// An agent hook judging a worktree sub-agent's call works in that worktree,
/// where the payload's `cwd` says the call was made.
#[tokio::test]
async fn an_agent_hook_judges_a_worktree_sub_agents_call_in_the_worktree() {
    let daemon = Daemon::start_with(Setup {
        git: true,
        settings: json!({"hooks_config": {"PreToolUse": [{
            "type": "agent",
            "prompt": "Judge the call",
            "if": "Glob",
        }]}}),
        ..Setup::default()
    })
    .await;
    daemon.model.push_turn(tool_round(
        "Agent",
        "toolu_agent",
        json!({
            "description": "work apart",
            "prompt": "Make a file, then look",
            "subagent_type": "general-purpose",
            "worktree": "judged",
        }),
    ));
    daemon.model.push_turn(tool_round(
        "Bash",
        "toolu_make",
        json!({ "command": "echo only in the worktree > only-here.txt" }),
    ));
    daemon.model.push_turn(tool_round(
        "Glob",
        "toolu_glob",
        json!({ "pattern": "*.txt" }),
    ));
    daemon.model.push_turn(tool_round(
        "Read",
        "toolu_judge_read",
        json!({ "file_path": "only-here.txt" }),
    ));
    daemon.model.push_turn(text_round(r#"{"ok": true}"#));
    daemon.model.push_turn(text_round("sub-agent over"));
    daemon.model.push_turn(text_round("done"));
    let client = daemon.connect().await;
    let session = daemon.session(&client, json!({})).await;
    let turn = daemon.run(&client, &session, "go").await;
    client.response(turn).await;
    let (read, is_error) = daemon.answer_to("toolu_judge_read");
    assert!(
        !is_error && read.contains("only in the worktree"),
        "the hook's agent did not work in the worktree: {read}"
    );
    let (glob, is_error) = daemon.answer_to("toolu_glob");
    assert!(!is_error && glob.contains("only-here.txt"), "{glob}");
}
