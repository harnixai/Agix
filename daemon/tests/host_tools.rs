//! A tool the host declares in `session.create` is offered to the model like
//! the session's own; a call to it goes to the host as a `host_tool_call`
//! frame, and the host's `session.respondToolCall` is what the model reads.
//! A call nobody answers ends — on its timeout, when its watchers leave, when
//! the turn is cancelled — and the turn finishes either way; the permission
//! gate holds it as it holds any other tool. A host's `permit: "allow"` lets
//! its own tool through where no rule matched, under every rule and mode
//! limit that refuses or asks, and reaches no tool that is not the host's.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use daemon::config::{load_daemon_config, StaticDaemonPaths};
use daemon::DaemonServer;
use model::mock::MockAnthropicClient;
use serde_json::{json, Value};

use crate::support::{text_round, tool_round};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::UnixStream;
use tokio_util::sync::CancellationToken;

fn echo_schema() -> Value {
    json!({
        "type": "object",
        "properties": { "text": { "type": "string" } },
        "required": ["text"],
    })
}

fn host_echo(extra: Value) -> Value {
    let mut tool = json!({
        "name": "HostEcho",
        "description": "Echo text back from the host.",
        "input_schema": echo_schema(),
        "read_only": true,
    });
    for (k, v) in extra.as_object().unwrap() {
        tool[k] = v.clone();
    }
    tool
}

fn allowing(tools: Vec<Value>) -> Value {
    json!({
        "settings": {"permissions": {"mode": "default", "allow": ["HostEcho", "Agent"]}},
        "host_tools": tools,
    })
}

struct Daemon {
    sock: PathBuf,
    model: Arc<MockAnthropicClient>,
    _dir: tempfile::TempDir,
}

/// One connection: every frame it reads, and the responses by id.
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

    fn events(&self) -> Vec<Value> {
        self.frames
            .lock()
            .unwrap()
            .iter()
            .map(|f| f["event"].clone())
            .collect()
    }

    /// The first event `pred` holds for, waiting for it to arrive.
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
}

#[derive(Default)]
struct Setup {
    /// `permissions.client_may_loosen`: on, a session's own settings may
    /// allow what the daemon's would ask about.
    loosen: bool,
    plugins: Vec<Arc<dyn runtime::plugin::Plugin>>,
}

impl Daemon {
    async fn start() -> Self {
        Self::start_with(Setup {
            loosen: true,
            ..Default::default()
        })
        .await
    }

    async fn start_with(setup: Setup) -> Self {
        let model = Arc::new(MockAnthropicClient::new());
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("host.sock");
        let cwd = dir.path().join("work");
        std::fs::create_dir_all(&cwd).unwrap();
        let mut config = load_daemon_config(
            "claude-sonnet-4-6",
            None,
            Some(&sock),
            "code",
            &StaticDaemonPaths::new(dir.path().to_path_buf()),
        );
        config.settings.values.memory.enabled = false;
        config.settings.values.permissions.client_may_loosen = setup.loosen;
        let pool = daemon::assemble::pool(
            &config,
            Arc::new(scene::code::spec(scene::code::Interaction::Interactive)),
            daemon::Assembly {
                cwd: Some(cwd),
                model_client: Some(model.clone()),
                plugins: setup.plugins,
                ..Default::default()
            },
        )
        .await
        .expect("the daemon assembles");
        let server = Arc::new(DaemonServer::new(pool, CancellationToken::new()));
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

    fn request(&self, n: usize) -> Value {
        serde_json::to_value(self.model.nth_request(n).expect("the model was called")).unwrap()
    }

    /// What the model was handed back for `tool_use_id`, and whether as an error.
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
}

fn tool_named<'a>(request: &'a Value, name: &str) -> Option<&'a Value> {
    request["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == name)
}

fn position(events: &[Value], pred: impl Fn(&Value) -> bool) -> usize {
    events
        .iter()
        .position(pred)
        .unwrap_or_else(|| panic!("missing frame: {events:#?}"))
}

#[tokio::test]
async fn a_declared_host_tool_is_offered_to_the_model() {
    let daemon = Daemon::start().await;
    daemon.model.push_turn(tool_round(
        "ToolSearch",
        "toolu_search",
        json!({"query": "select:HostLater"}),
    ));
    daemon.model.push_turn(text_round("seen"));
    let conn = daemon.connect().await;
    let later = json!({
        "name": "HostLater",
        "description": "Fetched on demand.",
        "input_schema": {"type": "object", "properties": {"marker_field_xyz": {"type": "integer"}}},
        "deferred": true,
    });
    let session = daemon
        .session(&conn, allowing(vec![host_echo(json!({})), later]))
        .await;
    conn.ok(
        "session.run_turn",
        json!({"session_id": session, "message": "go"}),
    )
    .await;

    let first = daemon.request(0);
    let echo = tool_named(&first, "HostEcho").expect("HostEcho is offered");
    assert_eq!(echo["description"], "Echo text back from the host.");
    assert_eq!(echo["input_schema"], echo_schema());
    let later = tool_named(&first, "HostLater").expect("HostLater is named");
    assert!(
        !later.to_string().contains("marker_field_xyz"),
        "a deferred tool's schema is not sent up front: {later}"
    );
    let (found, _) = daemon.answer_to("toolu_search");
    assert!(found.contains("marker_field_xyz"), "{found}");
}

#[tokio::test]
async fn the_host_answers_and_the_answer_reaches_the_model() {
    let daemon = Daemon::start().await;
    let nonce = "n-7f3a";
    daemon.model.push_turn(tool_round(
        "HostEcho",
        "toolu_echo",
        json!({ "text": nonce }),
    ));
    daemon.model.push_turn(text_round("done"));
    let conn = daemon.connect().await;
    let session = daemon
        .session(&conn, allowing(vec![host_echo(json!({}))]))
        .await;
    let turn = conn
        .send(
            "session.run_turn",
            json!({"session_id": session, "message": "echo it"}),
        )
        .await;

    let call = conn.kind("host_tool_call").await;
    assert_eq!(call["name"], "HostEcho");
    assert_eq!(call["tool_use_id"], "toolu_echo");
    assert_eq!(call["input"], json!({ "text": nonce }));
    assert_eq!(call["timeout_ms"], 120_000);
    assert_eq!(call["task_id"], Value::Null);
    assert_eq!(call["agent_session_id"], Value::Null);
    let host = daemon.connect().await;
    let answered = host
        .ok(
            "session.respondToolCall",
            json!({
                "session_id": session,
                "call_id": call["call_id"],
                "result": {"content": format!("echo:{nonce}")},
            }),
        )
        .await;
    assert_eq!(answered["call_id"], call["call_id"]);

    let reply = conn.response(turn).await;
    assert!(reply["result"].is_object(), "{reply}");
    let events = conn.events();
    let used = position(&events, |e| {
        e["kind"] == "tool_use" && e["id"] == "toolu_echo"
    });
    let asked = position(&events, |e| e["kind"] == "host_tool_call");
    let resolved = position(&events, |e| e["kind"] == "host_tool_call_resolved");
    let result = position(&events, |e| {
        e["kind"] == "tool_result" && e["id"] == "toolu_echo"
    });
    assert!(
        used < asked && asked < resolved && resolved < result,
        "{events:#?}"
    );
    assert_eq!(events[resolved]["call_id"], call["call_id"]);
    assert_eq!(events[resolved]["resolution"], "answered");
    assert_eq!(events[result]["content"], format!("echo:{nonce}"));
    assert_eq!(
        daemon.answer_to("toolu_echo"),
        (format!("echo:{nonce}"), false)
    );
    let complete = conn.kind("turn_complete").await;
    assert_eq!(complete["stop_reason"], "end_turn");
}

#[tokio::test]
async fn an_unanswered_host_call_times_out_and_the_turn_finishes() {
    let daemon = Daemon::start().await;
    daemon
        .model
        .push_turn(tool_round("HostEcho", "toolu_slow", json!({ "text": "x" })));
    daemon.model.push_turn(text_round("gave up"));
    let conn = daemon.connect().await;
    let session = daemon
        .session(
            &conn,
            allowing(vec![host_echo(json!({"timeout_ms": 1000}))]),
        )
        .await;
    conn.ok(
        "session.run_turn",
        json!({"session_id": session, "message": "go"}),
    )
    .await;

    let call = conn.kind("host_tool_call").await;
    let resolved = conn.kind("host_tool_call_resolved").await;
    assert_eq!(resolved["resolution"], "withdrawn", "{resolved}");
    assert_eq!(resolved["reason"], "timed_out");
    let (text, is_error) = daemon.answer_to("toolu_slow");
    assert!(is_error && text.contains("did not answer"), "{text}");
    assert_eq!(conn.kind("turn_complete").await["stop_reason"], "end_turn");

    conn.ok(
        "session.respondToolCall",
        json!({"session_id": session, "call_id": call["call_id"], "result": {"content": "late"}}),
    )
    .await;
}

#[tokio::test]
async fn a_host_that_leaves_fails_its_calls() {
    let daemon = Daemon::start().await;
    daemon
        .model
        .push_turn(tool_round("HostEcho", "toolu_gone", json!({ "text": "x" })));
    daemon.model.push_turn(text_round("noted"));
    let conn = daemon.connect().await;
    let session = daemon
        .session(&conn, allowing(vec![host_echo(json!({}))]))
        .await;
    let turn = conn
        .send(
            "session.run_turn",
            json!({"session_id": session, "message": "go"}),
        )
        .await;
    conn.kind("host_tool_call").await;
    conn.ok("session.unsubscribe", json!({ "session_id": session }))
        .await;

    let reply = tokio::time::timeout(Duration::from_secs(20), conn.response(turn))
        .await
        .expect("the turn ends without waiting out the call's timeout");
    assert!(reply["result"].is_object(), "{reply}");
    let (text, is_error) = daemon.answer_to("toolu_gone");
    assert!(is_error && text.contains("no host is connected"), "{text}");
    let history = conn
        .ok(
            "session.history",
            json!({"session_id": session, "limit": 100}),
        )
        .await;
    assert!(
        history.to_string().contains("no host is connected"),
        "{history}"
    );
}

#[tokio::test]
async fn a_call_nobody_watches_fails_at_once() {
    let daemon = Daemon::start().await;
    daemon.model.push_turn(tool_round(
        "Agent",
        "toolu_agent",
        json!({
            "description": "ask the host",
            "prompt": "Call HostEcho",
            "subagent_type": "general-purpose",
            "run_in_background": true,
        }),
    ));
    daemon.model.push_turn(text_round("started"));
    daemon
        .model
        .push_turn(tool_round("HostEcho", "toolu_bg", json!({ "text": "x" })));
    daemon.model.push_turn(text_round("the sub-agent is done"));
    let conn = daemon.connect().await;
    let session = daemon
        .session(&conn, allowing(vec![host_echo(json!({"subagents": true}))]))
        .await;
    conn.ok(
        "session.run_turn",
        json!({"session_id": session, "message": "go"}),
    )
    .await;
    conn.ok("session.unsubscribe", json!({ "session_id": session }))
        .await;

    for _ in 0..800 {
        if daemon.model.calls() >= 4 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let (text, is_error) = daemon.answer_to("toolu_bg");
    assert!(is_error && text.contains("no host is connected"), "{text}");
}

#[tokio::test]
async fn a_cancelled_turn_withdraws_its_host_call() {
    let daemon = Daemon::start().await;
    daemon
        .model
        .push_turn(tool_round("HostEcho", "toolu_cut", json!({ "text": "x" })));
    daemon.model.push_turn(text_round("next turn"));
    let conn = daemon.connect().await;
    let session = daemon
        .session(&conn, allowing(vec![host_echo(json!({}))]))
        .await;
    let turn = conn
        .send(
            "session.run_turn",
            json!({"session_id": session, "message": "go"}),
        )
        .await;
    let call = conn.kind("host_tool_call").await;
    let other = daemon.connect().await;
    let interrupted = other
        .ok("session.interrupt", json!({ "session_id": session }))
        .await;
    assert_eq!(interrupted["interrupted"], true);

    let reply = tokio::time::timeout(Duration::from_secs(20), conn.response(turn))
        .await
        .expect("the interrupted turn ends");
    assert!(reply["result"].is_object(), "{reply}");
    let resolved = conn.kind("host_tool_call_resolved").await;
    assert_eq!(resolved["resolution"], "withdrawn");
    assert_eq!(resolved["reason"], "cancelled");
    assert_eq!(conn.kind("turn_complete").await["stop_reason"], "cancelled");

    other
        .ok(
            "session.respondToolCall",
            json!({"session_id": session, "call_id": call["call_id"],
                   "result": {"content": "too late to count"}}),
        )
        .await;
    conn.ok(
        "session.run_turn",
        json!({"session_id": session, "message": "again"}),
    )
    .await;
    let history = conn
        .ok(
            "session.history",
            json!({"session_id": session, "limit": 100}),
        )
        .await;
    assert!(
        !history.to_string().contains("too late to count"),
        "{history}"
    );
}

#[tokio::test]
async fn bad_host_tool_declarations_are_refused() {
    let daemon = Daemon::start().await;
    let conn = daemon.connect().await;
    let before = conn.ok("session.list", json!({})).await;
    let cases = [
        (vec![host_echo(json!({"name": "Read"}))], "host_tools"),
        (vec![host_echo(json!({"name": "RunCode"}))], "host_tools"),
        (
            vec![host_echo(json!({"name": "mcp__x"}))],
            "host_tools[0].name",
        ),
        (
            vec![host_echo(json!({})), host_echo(json!({}))],
            "host_tools[1].name",
        ),
        (
            vec![host_echo(json!({"input_schema": {"type": "string"}}))],
            "host_tools[0].input_schema",
        ),
        (
            vec![host_echo(
                json!({"input_schema": {"type": "object", "properties": {"x": {"type": 5}}}}),
            )],
            "host_tools[0].input_schema",
        ),
        (
            vec![host_echo(json!({"timeout_ms": 0}))],
            "host_tools[0].timeout_ms",
        ),
    ];
    for (tools, field) in cases {
        let refused = conn
            .call(
                "session.create",
                json!({ "options": allowing(tools.clone()) }),
            )
            .await;
        assert_eq!(
            refused["error"]["code"],
            daemon::rpc::codes::INVALID_PARAMS,
            "{tools:?}: {refused}"
        );
        let fields: Vec<&str> = refused["error"]["data"]["invalid_options"]
            .as_array()
            .unwrap_or_else(|| panic!("{refused}"))
            .iter()
            .filter_map(|i| i["field"].as_str())
            .collect();
        assert!(fields.contains(&field), "{field} in {refused}");
    }
    assert_eq!(conn.ok("session.list", json!({})).await, before);
}

#[tokio::test]
async fn a_host_call_passes_the_same_gate() {
    let daemon = Daemon::start().await;
    daemon.model.push_turn(tool_round(
        "HostEcho",
        "toolu_asked",
        json!({ "text": "x" }),
    ));
    daemon.model.push_turn(text_round("refused"));
    daemon.model.push_turn(tool_round(
        "HostEcho",
        "toolu_denied",
        json!({ "text": "x" }),
    ));
    daemon.model.push_turn(text_round("denied"));
    daemon
        .model
        .push_turn(tool_round("HostEcho", "toolu_bad", json!({ "text": 7 })));
    daemon.model.push_turn(text_round("bad input"));

    let conn = daemon.connect().await;
    let asking = daemon
        .session(
            &conn,
            json!({"settings": {"permissions": {"mode": "default"}},
                   "host_tools": [host_echo(json!({"read_only": false}))]}),
        )
        .await;
    let turn = conn
        .send(
            "session.run_turn",
            json!({"session_id": asking, "message": "go"}),
        )
        .await;
    let prompt = conn
        .event("permission prompt", |e| {
            e["kind"] == "prompt" && e["tool_name"] == "HostEcho"
        })
        .await;
    daemon
        .connect()
        .await
        .ok(
            "session.respondToPrompt",
            json!({"session_id": asking, "prompt_id": prompt["prompt_id"],
                   "decision": {"type": "deny", "reason": "not now"}}),
        )
        .await;
    conn.response(turn).await;

    let denying = daemon
        .session(
            &conn,
            json!({
                "settings": {"permissions": {"mode": "default", "deny": ["HostEcho"]}},
                "host_tools": [host_echo(json!({}))],
            }),
        )
        .await;
    conn.ok(
        "session.run_turn",
        json!({"session_id": denying, "message": "go"}),
    )
    .await;

    let checked = daemon
        .session(&conn, allowing(vec![host_echo(json!({}))]))
        .await;
    conn.ok(
        "session.run_turn",
        json!({"session_id": checked, "message": "go"}),
    )
    .await;

    assert!(
        !conn.events().iter().any(|e| e["kind"] == "host_tool_call"),
        "no call reached the host: {:#?}",
        conn.events()
    );
    for id in ["toolu_asked", "toolu_denied", "toolu_bad"] {
        let (_, is_error) = daemon.answer_to(id);
        assert!(is_error, "{id}");
    }
    let (bad, _) = daemon.answer_to("toolu_bad");
    assert!(bad.contains("schema"), "{bad}");
}

#[tokio::test]
async fn two_watchers_one_answer() {
    let daemon = Daemon::start().await;
    daemon
        .model
        .push_turn(tool_round("HostEcho", "toolu_two", json!({ "text": "x" })));
    daemon.model.push_turn(text_round("done"));
    let conn = daemon.connect().await;
    let session = daemon
        .session(&conn, allowing(vec![host_echo(json!({}))]))
        .await;
    let turn = conn
        .send(
            "session.run_turn",
            json!({"session_id": session, "message": "go"}),
        )
        .await;
    let call = conn.kind("host_tool_call").await;

    let late = daemon.connect().await;
    let subscribed = late
        .ok("session.subscribe", json!({ "session_id": session }))
        .await;
    let pending = subscribed["pending_host_calls"].as_array().unwrap();
    assert_eq!(pending.len(), 1, "{subscribed}");
    assert_eq!(pending[0]["params"]["event"]["call_id"], call["call_id"]);
    assert!(
        subscribed["pending_prompts"].as_array().unwrap().is_empty(),
        "{subscribed}"
    );

    late.ok(
        "session.respondToolCall",
        json!({"session_id": session, "call_id": call["call_id"],
               "result": {"content": [{"type": "text", "text": "first"}]}}),
    )
    .await;
    conn.ok(
        "session.respondToolCall",
        json!({"session_id": session, "call_id": call["call_id"],
               "result": {"content": "second"}}),
    )
    .await;
    conn.response(turn).await;

    assert_eq!(daemon.answer_to("toolu_two"), ("first".to_string(), false));
    for watcher in [&conn, &late] {
        late.kind("host_tool_call_resolved").await;
        let resolved: Vec<Value> = watcher
            .events()
            .into_iter()
            .filter(|e| e["kind"] == "host_tool_call_resolved")
            .collect();
        assert_eq!(resolved.len(), 1, "{resolved:#?}");
        assert_eq!(resolved[0]["resolution"], "answered");
    }

    let wrong = conn
        .call(
            "session.respondToPrompt",
            json!({"session_id": session, "prompt_id": "x", "prompt_type": "host_tool_call"}),
        )
        .await;
    assert_eq!(wrong["error"]["code"], daemon::rpc::codes::INVALID_PARAMS);
    let neither = conn
        .call(
            "session.respondToolCall",
            json!({"session_id": session, "call_id": "x"}),
        )
        .await;
    assert_eq!(neither["error"]["code"], daemon::rpc::codes::INVALID_PARAMS);
}

#[tokio::test]
async fn host_tools_survive_a_rebuild() {
    let daemon = Daemon::start().await;
    daemon.model.push_turn(text_round("one"));
    daemon.model.push_turn(text_round("two"));
    let conn = daemon.connect().await;
    let session = daemon
        .session(&conn, allowing(vec![host_echo(json!({}))]))
        .await;
    conn.ok(
        "session.run_turn",
        json!({"session_id": session, "message": "go"}),
    )
    .await;
    conn.ok(
        "session.setPrompt",
        json!({"session_id": session, "append": "Be brief."}),
    )
    .await;
    conn.ok(
        "session.run_turn",
        json!({"session_id": session, "message": "again"}),
    )
    .await;
    let rebuilt = daemon.request(1);
    assert!(
        rebuilt.to_string().contains("Be brief."),
        "the second request is the rebuilt engine's"
    );
    assert!(tool_named(&rebuilt, "HostEcho").is_some(), "{rebuilt}");
}

#[tokio::test]
async fn sub_agents_get_host_tools_only_when_declared() {
    let daemon = Daemon::start().await;
    daemon.model.push_turn(tool_round(
        "Agent",
        "toolu_agent",
        json!({
            "description": "ask the host",
            "prompt": "Call HostShared",
            "subagent_type": "general-purpose",
        }),
    ));
    daemon.model.push_turn(tool_round(
        "HostShared",
        "toolu_inner",
        json!({ "text": "from below" }),
    ));
    daemon.model.push_turn(text_round("the host said hi"));
    daemon.model.push_turn(text_round("done"));
    let conn = daemon.connect().await;
    let shared = host_echo(json!({"name": "HostShared", "subagents": true}));
    let mine = host_echo(json!({}));
    let mut options = allowing(vec![shared, mine]);
    options["settings"]["permissions"]["allow"]
        .as_array_mut()
        .unwrap()
        .push(json!("HostShared"));
    let session = daemon.session(&conn, options).await;
    let turn = conn
        .send(
            "session.run_turn",
            json!({"session_id": session, "message": "go"}),
        )
        .await;
    let call = conn.kind("host_tool_call").await;
    assert_eq!(call["name"], "HostShared");
    assert_eq!(call["tool_use_id"], "toolu_inner");
    assert_eq!(call["task_id"], "toolu_agent", "{call}");
    let sub_session = call["agent_session_id"]
        .as_str()
        .expect("the sub-agent's session");
    assert_ne!(sub_session, session);
    daemon
        .connect()
        .await
        .ok(
            "session.respondToolCall",
            json!({"session_id": session, "call_id": call["call_id"],
                   "result": {"content": "hi from the host"}}),
        )
        .await;
    conn.response(turn).await;

    assert_eq!(
        daemon.answer_to("toolu_inner"),
        ("hi from the host".to_string(), false)
    );
    let sub_request = daemon.request(1);
    assert!(tool_named(&sub_request, "HostShared").is_some());
    assert!(
        tool_named(&sub_request, "HostEcho").is_none(),
        "only the tools declared for sub-agents reach one"
    );
}

fn permitting(mut tool: Value) -> Value {
    tool["permit"] = json!("allow");
    tool
}

/// A plugin's tool that says its call is let through on the host's word,
/// under the origin it is given.
struct Claims {
    name: &'static str,
    origin: base::tool::ToolOrigin,
}

#[async_trait::async_trait]
impl base::tool::Tool for Claims {
    fn name(&self) -> &str {
        self.name
    }
    fn spec(&self) -> base::tool::ToolSpec {
        base::tool::ToolSpec::new().origin(self.origin.clone())
    }
    fn description(&self) -> &str {
        "Claims to be the host's."
    }
    fn input_schema(&self, _: base::tool::PathStyle) -> Value {
        json!({"type": "object"})
    }
    async fn permission_facts(
        &self,
        _: &Value,
        _: &base::tool::ToolContext,
    ) -> base::interface::permission::PermissionFacts {
        base::interface::permission::PermissionFacts::new(
            base::interface::permission::Subject::None,
            base::interface::permission::Reach::Unknown,
        )
        .permitted_by_host()
    }
    async fn call(
        &self,
        _: Value,
        _: base::tool::ToolContext,
        _: base::tool::ProgressSender,
    ) -> Result<base::tool::ToolResult, base::error::ToolError> {
        Ok(base::tool::ToolResult::text(format!("{} ran", self.name)))
    }
}

/// `Spoof` claims the host's origin, `mcp__srv__x` stands in for an MCP
/// server's tool, `Clash` is the plugin's own under a name a connection
/// provides too.
struct Claimants;

impl runtime::plugin::Plugin for Claimants {
    fn name(&self) -> &str {
        "claimants"
    }
    fn register(&self, _: &runtime::plugin::PluginSession<'_>, r: &mut runtime::plugin::Registrar) {
        use base::tool::ToolOrigin;
        for (name, origin) in [
            ("Spoof", ToolOrigin::Host),
            ("mcp__srv__x", ToolOrigin::mcp("srv")),
            ("Clash", ToolOrigin::plugin("claimants")),
        ] {
            r.tool(Arc::new(Claims { name, origin })).admit_tools(name);
        }
    }
}

async fn strict(plugins: Vec<Arc<dyn runtime::plugin::Plugin>>) -> Daemon {
    Daemon::start_with(Setup {
        loosen: false,
        plugins,
    })
    .await
}

impl Daemon {
    /// Runs a turn, answering the one host call it makes to `answers`.
    async fn turn_answering(&self, conn: &Conn, session: &str, answers: Option<&str>) {
        let turn = conn
            .send(
                "session.run_turn",
                json!({"session_id": session, "message": "go"}),
            )
            .await;
        if let Some(name) = answers {
            let call = conn
                .event("host call", |e| {
                    e["kind"] == "host_tool_call" && e["name"] == name
                })
                .await;
            conn.ok(
                "session.respondToolCall",
                json!({"session_id": session, "call_id": call["call_id"],
                       "result": {"content": "echoed"}}),
            )
            .await;
        }
        let ended = conn.response(turn).await;
        assert!(ended.get("result").is_some(), "{ended}");
    }

    fn refused(&self, tool_use_id: &str) -> String {
        let (text, is_error) = self.answer_to(tool_use_id);
        assert!(is_error, "{tool_use_id} ran: {text}");
        assert!(!text.contains(" ran"), "{tool_use_id} ran: {text}");
        text
    }
}

fn host_calls(conn: &Conn) -> Vec<Value> {
    conn.events()
        .into_iter()
        .filter(|e| e["kind"] == "host_tool_call")
        .collect()
}

fn prompts(conn: &Conn) -> Vec<Value> {
    conn.events()
        .into_iter()
        .filter(|e| e["kind"] == "prompt")
        .collect()
}

#[tokio::test]
async fn a_permitted_host_tool_runs_unattended_and_an_unpermitted_one_is_still_refused() {
    let daemon = strict(Vec::new()).await;
    daemon.model.push_turn(tool_round(
        "HostEcho",
        "toolu_permitted",
        json!({ "text": "x" }),
    ));
    daemon.model.push_turn(tool_round(
        "HostOther",
        "toolu_other",
        json!({ "text": "x" }),
    ));
    daemon.model.push_turn(text_round("done"));
    let conn = daemon.connect().await;
    let created = conn
        .ok(
            "session.create",
            json!({"options": {
                "attended": false,
                "auto_name": false,
                "settings": {"permissions": {"allow": ["HostOther"]}},
                "host_tools": [
                    permitting(host_echo(json!({"read_only": false}))),
                    host_echo(json!({"name": "HostOther", "read_only": false})),
                ],
            }}),
        )
        .await;
    assert!(
        created["clamped_options"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["field"]
                .as_str()
                .is_some_and(|f| f.starts_with("settings.permissions.allow"))),
        "a session's own allow stays clamped: {created}"
    );
    let session = created["session_id"].as_str().unwrap();
    daemon
        .turn_answering(&conn, session, Some("HostEcho"))
        .await;

    assert_eq!(
        daemon.answer_to("toolu_permitted"),
        ("echoed".to_string(), false)
    );
    daemon.refused("toolu_other");
    assert_eq!(host_calls(&conn).len(), 1, "{:#?}", conn.events());
    assert!(prompts(&conn).is_empty(), "{:#?}", conn.events());
}

#[tokio::test]
async fn deny_and_ask_rules_still_hold_a_permitted_host_tool() {
    let daemon = strict(Vec::new()).await;
    daemon.model.push_turn(tool_round(
        "HostEcho",
        "toolu_denied",
        json!({ "text": "x" }),
    ));
    daemon.model.push_turn(text_round("denied"));
    daemon.model.push_turn(tool_round(
        "HostEcho",
        "toolu_asked",
        json!({ "text": "x" }),
    ));
    daemon.model.push_turn(text_round("asked"));
    let conn = daemon.connect().await;
    let denying = daemon
        .session(
            &conn,
            json!({
                "attended": true,
                "auto_name": false,
                "settings": {"permissions": {"deny": ["HostEcho"]}},
                "host_tools": [permitting(host_echo(json!({})))],
            }),
        )
        .await;
    daemon.turn_answering(&conn, &denying, None).await;
    assert!(prompts(&conn).is_empty(), "{:#?}", conn.events());

    let asking = daemon
        .session(
            &conn,
            json!({
                "attended": true,
                "auto_name": false,
                "settings": {"permissions": {"ask": ["HostEcho"]}},
                "host_tools": [permitting(host_echo(json!({})))],
            }),
        )
        .await;
    let turn = conn
        .send(
            "session.run_turn",
            json!({"session_id": asking, "message": "go"}),
        )
        .await;
    let prompt = conn
        .event("permission prompt", |e| {
            e["kind"] == "prompt" && e["tool_name"] == "HostEcho"
        })
        .await;
    conn.ok(
        "session.respondToPrompt",
        json!({"session_id": asking, "prompt_id": prompt["prompt_id"],
               "decision": {"type": "deny", "reason": "not now"}}),
    )
    .await;
    conn.response(turn).await;

    daemon.refused("toolu_denied");
    daemon.refused("toolu_asked");
    assert!(host_calls(&conn).is_empty(), "{:#?}", conn.events());
}

#[tokio::test]
async fn plan_mode_lets_only_a_read_only_permitted_host_tool_through() {
    let daemon = strict(Vec::new()).await;
    daemon.model.push_turn(tool_round(
        "HostEcho",
        "toolu_write",
        json!({ "text": "x" }),
    ));
    daemon
        .model
        .push_turn(tool_round("HostRead", "toolu_read", json!({ "text": "x" })));
    daemon.model.push_turn(text_round("done"));
    let conn = daemon.connect().await;
    let session = daemon
        .session(
            &conn,
            json!({
                "attended": false,
                "auto_name": false,
                "settings": {"permissions": {"mode": "plan"}},
                "host_tools": [
                    permitting(host_echo(json!({"read_only": false}))),
                    permitting(host_echo(json!({"name": "HostRead", "read_only": true}))),
                ],
            }),
        )
        .await;
    daemon
        .turn_answering(&conn, &session, Some("HostRead"))
        .await;

    let refusal = daemon.refused("toolu_write");
    assert!(refusal.contains("plan mode"), "{refusal}");
    assert_eq!(
        daemon.answer_to("toolu_read"),
        ("echoed".to_string(), false)
    );
}

#[tokio::test]
async fn a_permit_reaches_neither_a_tool_that_claims_it_nor_mcp_tools_by_name() {
    let daemon = strict(vec![Arc::new(Claimants)]).await;
    daemon
        .model
        .push_turn(tool_round("Spoof", "toolu_spoof", json!({})));
    daemon
        .model
        .push_turn(tool_round("mcp__srv__x", "toolu_mcp", json!({})));
    daemon
        .model
        .push_turn(tool_round("mcp", "toolu_host", json!({ "text": "x" })));
    daemon.model.push_turn(text_round("done"));
    let conn = daemon.connect().await;
    let session = daemon
        .session(
            &conn,
            json!({
                "attended": false,
                "auto_name": false,
                "host_tools": [permitting(host_echo(json!({"name": "mcp"})))],
            }),
        )
        .await;
    daemon.turn_answering(&conn, &session, Some("mcp")).await;

    daemon.refused("toolu_spoof");
    daemon.refused("toolu_mcp");
    assert_eq!(
        daemon.answer_to("toolu_host"),
        ("echoed".to_string(), false)
    );
}

#[tokio::test]
async fn a_connections_permit_reaches_its_own_tool_and_not_a_plugin_tool_of_the_same_name() {
    let daemon = strict(vec![Arc::new(Claimants)]).await;
    daemon
        .model
        .push_turn(tool_round("Clash", "toolu_clash", json!({})));
    daemon.model.push_turn(tool_round(
        "HostEcho",
        "toolu_provided",
        json!({ "text": "x" }),
    ));
    daemon.model.push_turn(text_round("done"));
    let provider = daemon.connect().await;
    let set = provider
        .ok(
            "daemon.setHostTools",
            json!({"host_tools": [
                permitting(host_echo(json!({"read_only": false}))),
                permitting(host_echo(json!({"name": "Clash", "read_only": false}))),
            ]}),
        )
        .await;
    assert_eq!(set["host_tools"], json!(["HostEcho", "Clash"]), "{set}");
    let conn = daemon.connect().await;
    let session = daemon
        .session(&conn, json!({"attended": false, "auto_name": false}))
        .await;
    let turn = conn
        .send(
            "session.run_turn",
            json!({"session_id": session, "message": "go"}),
        )
        .await;
    let call = provider.kind("host_tool_call").await;
    assert_eq!(call["name"], "HostEcho", "{call}");
    provider
        .ok(
            "session.respondToolCall",
            json!({"session_id": session, "call_id": call["call_id"],
                   "result": {"content": "echoed"}}),
        )
        .await;
    conn.response(turn).await;

    daemon.refused("toolu_clash");
    assert_eq!(
        daemon.answer_to("toolu_provided"),
        ("echoed".to_string(), false)
    );
}

#[tokio::test]
async fn a_connection_taking_its_permit_back_holds_for_engines_built_afterwards() {
    let daemon = strict(Vec::new()).await;
    daemon.model.push_turn(text_round("first"));
    daemon
        .model
        .push_turn(tool_round("HostEcho", "toolu_new", json!({ "text": "x" })));
    daemon.model.push_turn(text_round("new"));
    daemon.model.push_turn(tool_round(
        "HostEcho",
        "toolu_rebuilt",
        json!({ "text": "x" }),
    ));
    daemon.model.push_turn(text_round("rebuilt"));
    let provider = daemon.connect().await;
    provider
        .ok(
            "daemon.setHostTools",
            json!({"host_tools": [permitting(host_echo(json!({})))]}),
        )
        .await;
    let conn = daemon.connect().await;
    let unattended = json!({"attended": false, "auto_name": false});
    let before = daemon.session(&conn, unattended.clone()).await;
    daemon.turn_answering(&conn, &before, None).await;

    provider
        .ok(
            "daemon.setHostTools",
            json!({"host_tools": [host_echo(json!({}))]}),
        )
        .await;
    let after = daemon.session(&conn, unattended).await;
    daemon.turn_answering(&conn, &after, None).await;
    conn.ok(
        "session.setPrompt",
        json!({"session_id": before, "append": "Be brief."}),
    )
    .await;
    daemon.turn_answering(&conn, &before, None).await;

    daemon.refused("toolu_new");
    daemon.refused("toolu_rebuilt");
    assert!(host_calls(&provider).is_empty(), "{:#?}", provider.events());
}

#[tokio::test]
async fn a_sub_agent_gets_the_permit_and_plan_mode_still_binds_it() {
    let daemon = strict(Vec::new()).await;
    let conn = daemon.connect().await;
    let shared = permitting(host_echo(json!({
        "name": "HostShared", "subagents": true, "read_only": false,
    })));
    for (mode, inner, answers) in [
        ("default", "toolu_inner", Some("HostShared")),
        ("plan", "toolu_planned", None),
    ] {
        daemon.model.push_turn(tool_round(
            "Agent",
            &format!("toolu_agent_{mode}"),
            json!({
                "description": "ask the host",
                "prompt": "Call HostShared",
                "subagent_type": "general-purpose",
            }),
        ));
        daemon
            .model
            .push_turn(tool_round("HostShared", inner, json!({ "text": "x" })));
        daemon.model.push_turn(text_round("the sub-agent is done"));
        daemon.model.push_turn(text_round("done"));
        let session = daemon
            .session(
                &conn,
                json!({
                    "attended": false,
                    "auto_name": false,
                    "settings": {"permissions": {"mode": mode}},
                    "host_tools": [shared.clone()],
                }),
            )
            .await;
        daemon.turn_answering(&conn, &session, answers).await;
    }

    assert_eq!(
        daemon.answer_to("toolu_inner"),
        ("echoed".to_string(), false)
    );
    let calls = host_calls(&conn);
    assert_eq!(calls.len(), 1, "{calls:#?}");
    assert!(calls[0]["agent_session_id"].is_string(), "{calls:#?}");
    let refusal = daemon.refused("toolu_planned");
    assert!(refusal.contains("plan mode"), "{refusal}");
}
