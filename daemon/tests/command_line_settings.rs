//! `agixd --set KEY=VALUE` seen from outside: a real `agixd` process whose
//! model is an HTTP stub. The values are a `cli` layer over the settings files
//! of every project and scene, still there after `config.reload`, and reported
//! as `cli` by `config.get` and `daemon.doctor`; `--max-tokens` and
//! `--session-cap` are shorthands; a key no setting reads, an old key or a
//! value that does not fit keeps the daemon from starting and says which
//! `--set` it was. `app.session_cap` holds from the global settings too, and
//! stays what the daemon started with across `config.reload`.

use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use rpc_client::DaemonRpcClient;
use serde_json::{json, Value};

use crate::list_models::{sse_text, Seen, Stub};

struct World {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

impl World {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("home/.agix")).unwrap();
        let world = Self { _dir: dir, root };
        world.project("work");
        world
    }

    fn data_root(&self) -> PathBuf {
        self.root.join("home/.agix")
    }

    /// The project the daemon runs in.
    fn work(&self) -> PathBuf {
        self.root.join("work")
    }

    fn project(&self, name: &str) -> PathBuf {
        let dir = self.root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_global(&self, settings: Value) {
        std::fs::write(self.data_root().join("settings.json"), settings.to_string()).unwrap();
    }

    fn write_project(&self, project: &Path, settings: Value) {
        std::fs::create_dir_all(project.join(".agix")).unwrap();
        std::fs::write(project.join(".agix/settings.json"), settings.to_string()).unwrap();
    }

    fn command(&self, stub: &Stub, args: &[&str]) -> (tokio::process::Command, PathBuf, PathBuf) {
        let sock = self.root.join("d.sock");
        let stderr = self.root.join("agixd.stderr");
        let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_agixd"));
        cmd.arg("--socket")
            .arg(&sock)
            .arg("--instance")
            .arg("command-line-settings")
            .args(args)
            .current_dir(self.work())
            .env("HOME", self.root.join("home"))
            .env("AGIX_CONFIG_HOME", self.data_root())
            .env("ANTHROPIC_BASE_URL", &stub.base)
            .env("ANTHROPIC_API_KEY", "stub")
            .env_remove("ANTHROPIC_AUTH_TOKEN")
            .env_remove("AGIX_DAEMON_TOKEN")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(&stderr).unwrap())
            .kill_on_drop(true);
        (cmd, sock, stderr)
    }
}

struct Agixd {
    _child: tokio::process::Child,
    sock: PathBuf,
}

impl Agixd {
    async fn start(world: &World, stub: &Stub, args: &[&str]) -> Self {
        let (mut cmd, sock, stderr) = world.command(stub, args);
        let mut child = cmd.spawn().expect("agixd starts");
        for _ in 0..2400 {
            if let Ok(mut client) = DaemonRpcClient::connect(&sock).await {
                if client
                    .call("daemon.ping", Value::Null)
                    .await
                    .is_ok_and(|r| r.error.is_none())
                {
                    return Self {
                        _child: child,
                        sock,
                    };
                }
            }
            if let Ok(Some(status)) = child.try_wait() {
                panic!(
                    "agixd {args:?} exited at startup ({status}):\n{}",
                    std::fs::read_to_string(&stderr).unwrap_or_default()
                );
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("agixd {args:?} never answered");
    }

    async fn client(&self) -> DaemonRpcClient {
        DaemonRpcClient::connect(&self.sock).await.unwrap()
    }

    async fn ok(&self, method: &str, params: Value) -> Value {
        let response = self.client().await.call(method, params).await.unwrap();
        assert!(response.error.is_none(), "{method}: {:?}", response.error);
        response.result.unwrap_or_default()
    }
}

/// How `agixd` with `args` ended, and what it wrote to stderr, when it is
/// expected not to start.
async fn refused(args: &[&str]) -> (ExitStatus, String) {
    let world = World::new();
    let stub = Stub::start(|_| sse_text("unused")).await;
    let (mut cmd, _, stderr) = world.command(&stub, args);
    let mut child = cmd.spawn().expect("agixd starts");
    let status = tokio::time::timeout(Duration::from_secs(60), child.wait())
        .await
        .unwrap_or_else(|_| panic!("agixd {args:?} kept running"))
        .unwrap();
    (status, std::fs::read_to_string(stderr).unwrap_or_default())
}

fn sse_tool_use(id: &str, name: &str, input: Value) -> String {
    let events = [
        json!({"type": "message_start", "message": {"id": "msg", "model": "m", "role": "assistant",
            "usage": {"input_tokens": 3, "output_tokens": 0}}}),
        json!({"type": "content_block_start", "index": 0,
            "content_block": {"type": "tool_use", "id": id, "name": name, "input": {}}}),
        json!({"type": "content_block_delta", "index": 0,
            "delta": {"type": "input_json_delta", "partial_json": input.to_string()}}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 1}}),
        json!({"type": "message_stop"}),
    ];
    let mut out = String::from(
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n",
    );
    for e in events {
        out.push_str(&format!(
            "event: {}\ndata: {e}\n\n",
            e["type"].as_str().unwrap()
        ));
    }
    out
}

/// A request of a session's turn, as opposed to one naming the session.
fn of_a_turn(request: &Seen) -> bool {
    request.path.ends_with("/v1/messages")
        && request.body["tools"]
            .as_array()
            .is_some_and(|tools| !tools.is_empty())
}

fn answered_tool(request: &Seen) -> bool {
    request.body["messages"]
        .as_array()
        .and_then(|messages| messages.last())
        .is_some_and(|last| last.to_string().contains("tool_result"))
}

/// A model that writes `made.txt` in `project`, then says it is done.
async fn writes_into(project: &Path) -> Stub {
    let target = project.join("made.txt").display().to_string();
    Stub::answering(move |request| {
        if of_a_turn(request) && !answered_tool(request) {
            sse_tool_use("w1", "Write", json!({"file_path": target, "content": "x"}))
        } else {
            sse_text("done")
        }
    })
    .await
}

fn no_memory() -> Value {
    json!({"memory": {"enabled": false}})
}

#[tokio::test]
async fn set_permissions_mode_makes_new_sessions_plan_and_a_write_is_refused() {
    let world = World::new();
    world.write_global(no_memory());
    let stub = writes_into(&world.work()).await;
    let daemon = Agixd::start(&world, &stub, &["--set", "permissions.mode=plan"]).await;

    let mut client = daemon.client().await;
    let session = client.session_create(json!({})).await.unwrap();
    let got = client
        .call("session.resume", json!({"session_id": session}))
        .await
        .unwrap();
    assert_eq!(
        got.result.as_ref().unwrap()["permission"]["mode"],
        "plan",
        "{got:?}"
    );
    let turn = client
        .session_run_turn(&session, "write the file", "t1", None)
        .await
        .unwrap();
    assert!(
        turn.tool_results
            .first()
            .is_some_and(|(_, _, is_error)| *is_error),
        "{:?}",
        turn.tool_results
    );
    assert!(!world.work().join("made.txt").exists());
}

#[tokio::test]
async fn set_turn_max_model_calls_holds_a_turn_to_one_model_call() {
    let world = World::new();
    world.write_global(no_memory());
    let stub = Stub::answering(|request| {
        if of_a_turn(request) {
            sse_tool_use("g", "Glob", json!({"pattern": "*"}))
        } else {
            sse_text("title")
        }
    })
    .await;
    let daemon = Agixd::start(&world, &stub, &["--set", "turn.max_model_calls=1"]).await;

    let mut client = daemon.client().await;
    let session = client.session_create(json!({})).await.unwrap();
    let _ = client
        .session_run_turn(&session, "look around", "t1", None)
        .await;
    let calls = stub.seen().iter().filter(|r| of_a_turn(r)).count();
    assert_eq!(calls, 1, "the turn's model calls");
}

#[tokio::test]
async fn set_app_history_keeps_a_trusted_projects_sessions_in_it() {
    let world = World::new();
    world.write_global(json!({
        "memory": {"enabled": false},
        "app": {"trusted_projects": [world.work()]},
    }));
    let stub = Stub::start(|_| sse_text("noted")).await;
    let daemon = Agixd::start(&world, &stub, &["--set", "app.history=project"]).await;

    let mut client = daemon.client().await;
    let session = client.session_create(json!({})).await.unwrap();
    let turn = client
        .session_run_turn(&session, "remember", "t1", None)
        .await
        .unwrap();
    assert!(turn.turn_complete, "{turn:?}");
    let kept = world.work().join(".agix/state/history").join(&session);
    assert!(kept.is_dir(), "{} is not there", kept.display());
}

#[tokio::test]
async fn set_permission_rules_reach_a_pair_that_is_not_the_daemons_own() {
    let world = World::new();
    world.write_global(json!({
        "memory": {"enabled": false},
        "permissions": {"mode": "acceptEdits"},
    }));
    let other = world.project("other");
    let stub = writes_into(&other).await;
    let daemon = Agixd::start(&world, &stub, &["--set", r#"permissions.deny=["Write"]"#]).await;

    let mut client = daemon.client().await;
    let session = client
        .session_create(json!({"project_root": other}))
        .await
        .unwrap();
    let turn = client
        .session_run_turn(&session, "write the file", "t1", None)
        .await
        .unwrap();
    assert!(
        turn.tool_results
            .first()
            .is_some_and(|(_, _, is_error)| *is_error),
        "{:?}",
        turn.tool_results
    );
    assert!(!other.join("made.txt").exists());
}

#[tokio::test]
async fn an_unknown_key_an_old_key_and_a_value_that_does_not_fit_refuse_to_start() {
    let (status, stderr) = refused(&["--set", "turn.max_model_calz=1"]).await;
    assert!(!status.success(), "{status}");
    assert!(
        stderr.contains("--set turn.max_model_calz=1") && stderr.contains("no setting reads"),
        "{stderr}"
    );

    let (status, stderr) = refused(&["--set", "permission_mode=plan"]).await;
    assert!(!status.success(), "{status}");
    assert!(
        stderr.contains("--set permission_mode=plan") && stderr.contains("permissions.mode"),
        "{stderr}"
    );

    let (status, stderr) = refused(&["--set", "turn.max_model_calls=abc"]).await;
    assert!(!status.success(), "{status}");
    assert!(
        stderr.contains("--set turn.max_model_calls=abc") && stderr.contains("invalid type"),
        "{stderr}"
    );
}

#[tokio::test]
async fn the_cli_layer_outlasts_a_reload_and_config_get_and_doctor_report_it() {
    let world = World::new();
    world.write_global(json!({"memory": {"enabled": false}}));
    let stub = Stub::start(|_| sse_text("ok")).await;
    let daemon = Agixd::start(
        &world,
        &stub,
        &[
            "--set",
            "permissions.mode=plan",
            "--set",
            "turn.max_model_calls=3",
        ],
    )
    .await;

    world.write_global(json!({
        "memory": {"enabled": false},
        "permissions": {"mode": "acceptEdits"},
        "turn": {"max_model_calls": 7},
    }));
    daemon.ok("config.reload", json!({})).await;
    let effective = daemon
        .ok("config.get", json!({"scene": "code", "tier": "effective"}))
        .await;
    assert_eq!(
        effective["settings"]["permissions"]["mode"], "plan",
        "{effective}"
    );
    assert_eq!(
        effective["settings"]["turn"]["max_model_calls"], 3,
        "{effective}"
    );
    let session = daemon.ok("session.create", json!({})).await;
    let session = session["session_id"].as_str().unwrap();
    let got = daemon
        .ok("session.resume", json!({"session_id": session}))
        .await;
    assert_eq!(got["permission"]["mode"], "plan", "{got}");

    let cli = daemon
        .ok("config.get", json!({"scene": "code", "tier": "cli"}))
        .await;
    assert_eq!(cli["tier"], "cli");
    assert_eq!(
        cli["settings"],
        json!({"permissions": {"mode": "plan"}, "turn": {"max_model_calls": 3}})
    );

    let doctor = daemon.ok("daemon.doctor", json!({})).await;
    let tier = doctor["settings_tiers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["tier"] == "cli")
        .cloned()
        .unwrap_or_else(|| panic!("no cli tier: {doctor}"));
    assert_eq!(
        tier["keys"],
        json!(["permissions.mode", "turn.max_model_calls"]),
        "{tier}"
    );
}

/// The `max_tokens` the turns of a session on `project` ask for.
async fn max_tokens_asked(daemon: &Agixd, stub: &Stub, project: &Path) -> Vec<Value> {
    let mut client = daemon.client().await;
    let session = client
        .session_create(json!({"project_root": project}))
        .await
        .unwrap();
    let needle = format!("MSG-{}", project.display());
    client
        .session_run_turn(&session, &needle, "t1", None)
        .await
        .unwrap();
    stub.seen()
        .iter()
        .filter(|r| of_a_turn(r) && r.body.to_string().contains(&needle))
        .map(|r| r.body["max_tokens"].clone())
        .collect()
}

#[tokio::test]
async fn max_tokens_overrides_every_settings_file_as_its_set_form_does() {
    for args in [["--max-tokens", "1234"], ["--set", "model.max_tokens=1234"]] {
        let world = World::new();
        world.write_global(json!({
            "memory": {"enabled": false},
            "model": {"max_tokens": 1000},
        }));
        let other = world.project("other");
        world.write_project(&other, json!({"model": {"max_tokens": 4321}}));
        let stub = Stub::start(|_| sse_text("ok")).await;
        let daemon = Agixd::start(&world, &stub, &args).await;

        assert_eq!(
            max_tokens_asked(&daemon, &stub, &world.work()).await,
            [json!(1234)],
            "{args:?}"
        );
        assert_eq!(
            max_tokens_asked(&daemon, &stub, &other).await,
            [json!(1234)],
            "{args:?}"
        );
    }
}

/// Whether a session that took a turn is still in memory after a second
/// session took one.
async fn first_session_stays_with_a_second(daemon: &Agixd) -> bool {
    let mut client = daemon.client().await;
    let first = client.session_create(json!({})).await.unwrap();
    client
        .session_run_turn(&first, "one", "t1", None)
        .await
        .unwrap();
    let second = client.session_create(json!({})).await.unwrap();
    client
        .session_run_turn(&second, "two", "t1", None)
        .await
        .unwrap();
    let got = daemon.ok("session.get", json!({"session_id": first})).await;
    match got["status"].as_str() {
        Some("active") => true,
        Some("inactive") => false,
        _ => panic!("no status: {got}"),
    }
}

fn with_session_cap(cap: u64) -> Value {
    json!({"memory": {"enabled": false}, "app": {"session_cap": cap}})
}

#[tokio::test]
async fn app_session_cap_in_the_global_settings_holds_the_daemon_to_one_session() {
    let stub = Stub::start(|_| sse_text("ok")).await;
    let world = World::new();
    world.write_global(no_memory());
    let daemon = Agixd::start(&world, &stub, &[]).await;
    assert!(first_session_stays_with_a_second(&daemon).await);

    let world = World::new();
    world.write_global(with_session_cap(1));
    let daemon = Agixd::start(&world, &stub, &[]).await;
    assert!(!first_session_stays_with_a_second(&daemon).await);
}

#[tokio::test]
async fn session_cap_is_its_set_form_over_the_global_settings() {
    for args in [["--session-cap", "1"], ["--set", "app.session_cap=1"]] {
        let world = World::new();
        world.write_global(with_session_cap(8));
        let stub = Stub::start(|_| sse_text("ok")).await;
        let daemon = Agixd::start(&world, &stub, &args).await;
        assert!(
            !first_session_stays_with_a_second(&daemon).await,
            "{args:?}"
        );
    }
}

#[tokio::test]
async fn a_reload_leaves_the_session_cap_the_daemon_started_with() {
    let world = World::new();
    world.write_global(with_session_cap(1));
    let stub = Stub::start(|_| sse_text("ok")).await;
    let daemon = Agixd::start(&world, &stub, &[]).await;

    world.write_global(with_session_cap(8));
    daemon.ok("config.reload", json!({})).await;
    assert!(!first_session_stays_with_a_second(&daemon).await);
}

#[tokio::test]
async fn worker_threads_and_its_set_form_refuse_a_count_of_zero() {
    let (status, stderr) = refused(&["--worker-threads", "0"]).await;
    assert!(!status.success(), "{status}");
    assert!(stderr.contains("invalid worker thread count"), "{stderr}");

    let (status, stderr) = refused(&["--set", "app.worker_threads=0"]).await;
    assert!(!status.success(), "{status}");
    assert!(stderr.contains("--set app.worker_threads=0"), "{stderr}");
}
