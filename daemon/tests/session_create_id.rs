//! `session.create` with a caller's `session_id` creates that session once.
//! Asking again — on another connection at the same moment, or after the
//! daemon restarted — answers the session already there, with its history;
//! an id that is some other session's is refused and changes nothing.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use daemon::config::{load_daemon_config, StaticDaemonPaths};
use daemon::DaemonServer;
use model::client::AnthropicClient;
use model::mock::MockAnthropicClient;
use tokio_util::sync::CancellationToken;

use crate::support::{call_frame, text_round};

struct Daemon {
    sock: PathBuf,
    model: Arc<MockAnthropicClient>,
    cancel: CancellationToken,
    serving: tokio::task::JoinHandle<()>,
}

impl Daemon {
    /// A daemon over `root` keeping its transcripts there; a second one over
    /// the same root, once the first has stopped, is a restart.
    async fn start(root: &Path, name: &str, turns: usize) -> Self {
        let model = Arc::new(MockAnthropicClient::new());
        for i in 0..turns {
            model.push_turn(text_round(&format!("answer {i}")));
        }
        let sock = root.join(format!("{name}.sock"));
        let cwd = root.join("work");
        std::fs::create_dir_all(&cwd).unwrap();
        let mut config = load_daemon_config(
            "claude-sonnet-4-6",
            2000,
            Some(&sock),
            "code",
            &StaticDaemonPaths::new(root.to_path_buf()),
        );
        config.settings.values.memory.enabled = false;
        crate::support::allow_tools(&mut config, &[]);
        let pool = daemon::assemble::pool(
            &config,
            Arc::new(scene::code::spec(scene::code::Interaction::Interactive)),
            daemon::Assembly {
                cwd: Some(cwd),
                model_client: Some(model.clone() as Arc<dyn AnthropicClient>),
                ..Default::default()
            },
        )
        .await
        .expect("the daemon assembles");
        let cancel = CancellationToken::new();
        let server = Arc::new(DaemonServer::new(pool, cancel.clone()));
        let bind = sock.clone();
        let serving = tokio::spawn(async move {
            let _ = server.serve_unix(&bind).await;
        });
        for _ in 0..80 {
            if sock.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        assert!(sock.exists(), "socket never bound");
        Self {
            sock,
            model,
            cancel,
            serving,
        }
    }

    async fn stop(self) {
        self.cancel.cancel();
        let _ = self.serving.await;
    }

    async fn call(&self, method: &str, params: serde_json::Value) -> serde_json::Value {
        call_frame(&self.sock, method, params).await
    }

    async fn ok(&self, method: &str, params: serde_json::Value) -> serde_json::Value {
        let response = self.call(method, params).await;
        assert!(
            response.get("result").is_some(),
            "{method} failed: {response}"
        );
        response["result"].clone()
    }

    async fn listed(&self, id: &str) -> usize {
        let list = self
            .ok(
                "session.list",
                serde_json::json!({"include_children": true}),
            )
            .await;
        list["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|s| s["session_id"] == id)
            .count()
    }
}

fn fresh_id() -> String {
    base::session::SessionId::new().to_string()
}

#[tokio::test]
async fn one_id_asked_for_twice_is_one_session() {
    let dir = tempfile::tempdir().unwrap();
    let daemon = Daemon::start(dir.path(), "d", 1).await;
    let id = fresh_id();

    let first = daemon
        .ok("session.create", serde_json::json!({"session_id": id}))
        .await;
    assert_eq!(first["session_id"], id.as_str());
    assert_eq!(first["existing"], false, "{first}");
    let second = daemon
        .ok("session.create", serde_json::json!({"session_id": id}))
        .await;
    assert_eq!(second["session_id"], id.as_str());
    assert_eq!(second["existing"], true, "{second}");
    assert_eq!(daemon.listed(&id).await, 1);

    let raced = fresh_id();
    let ask = || daemon.call("session.create", serde_json::json!({"session_id": raced}));
    let (a, b) = tokio::join!(ask(), ask());
    let mut existing = [
        a["result"]["existing"].clone(),
        b["result"]["existing"].clone(),
    ];
    existing.sort_by_key(|v| v.as_bool());
    assert_eq!(
        existing,
        [serde_json::json!(false), serde_json::json!(true)],
        "of two connections asking at once, one creates and one finds: {a} {b}"
    );
    assert_eq!(daemon.listed(&raced).await, 1);

    daemon
        .ok(
            "session.run_turn",
            serde_json::json!({"session_id": id, "message": "MSG-ON-THE-ONE"}),
        )
        .await;
    daemon.stop().await;
}

#[tokio::test]
async fn after_a_restart_the_id_is_the_session_it_was_with_its_history() {
    let dir = tempfile::tempdir().unwrap();
    let id = fresh_id();
    let before = Daemon::start(dir.path(), "before", 1).await;
    before
        .ok("session.create", serde_json::json!({"session_id": id}))
        .await;
    before
        .ok(
            "session.run_turn",
            serde_json::json!({"session_id": id, "message": "MSG-BEFORE-RESTART"}),
        )
        .await;
    before.stop().await;

    let after = Daemon::start(dir.path(), "after", 1).await;
    let again = after
        .ok("session.create", serde_json::json!({"session_id": id}))
        .await;
    assert_eq!(again["existing"], true, "{again}");
    assert_eq!(after.listed(&id).await, 1);
    let history = after
        .ok("session.history", serde_json::json!({"session_id": id}))
        .await;
    assert!(
        history.to_string().contains("MSG-BEFORE-RESTART"),
        "the transcript is the one written before the restart: {history}"
    );

    after
        .ok(
            "session.run_turn",
            serde_json::json!({"session_id": id, "message": "MSG-AFTER-RESTART"}),
        )
        .await;
    let request = after
        .model
        .nth_request(0)
        .expect("the turn reached the model");
    let rendered = serde_json::to_string(&request).unwrap();
    assert!(
        rendered.contains("MSG-BEFORE-RESTART") && rendered.contains("MSG-AFTER-RESTART"),
        "the session found goes on from where it was: {rendered}"
    );
    after.stop().await;
}

#[tokio::test]
async fn an_id_that_is_another_scenes_session_is_refused_and_left_alone() {
    let dir = tempfile::tempdir().unwrap();
    let daemon = Daemon::start(dir.path(), "d", 0).await;
    let id = fresh_id();
    let created = daemon
        .ok("session.create", serde_json::json!({"session_id": id}))
        .await;
    daemon
        .ok("scene.activate", serde_json::json!({"scene": "harnix"}))
        .await;

    let refused = daemon
        .call(
            "session.create",
            serde_json::json!({"session_id": id, "scene": "harnix", "project_root": null}),
        )
        .await;
    assert_eq!(
        refused["error"]["code"], -32025,
        "SESSION_EXISTS: {refused}"
    );
    let data = &refused["error"]["data"];
    assert_eq!(data["session_id"], id.as_str());
    assert_eq!(data["scene"], "code", "{refused}");
    assert_eq!(data["project_root"], created["project_root"], "{refused}");
    assert_eq!(data["session_kind"], "primary", "{refused}");

    let elsewhere = dir.path().join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    let refused = daemon
        .call(
            "session.create",
            serde_json::json!({"session_id": id, "project_root": elsewhere}),
        )
        .await;
    assert_eq!(
        refused["error"]["code"], -32025,
        "another project is another session too: {refused}"
    );

    let got = daemon
        .ok("session.get", serde_json::json!({"session_id": id}))
        .await;
    assert_eq!(got["scene"], "code");
    assert_eq!(got["project_root"], created["project_root"]);
    daemon.stop().await;
}

#[tokio::test]
async fn an_id_not_in_the_engines_form_is_invalid_params() {
    let dir = tempfile::tempdir().unwrap();
    let daemon = Daemon::start(dir.path(), "d", 0).await;
    for id in ["not-an-id", "", "0OIl"] {
        let response = daemon
            .call("session.create", serde_json::json!({"session_id": id}))
            .await;
        assert_eq!(
            response["error"]["code"], -32602,
            "INVALID_PARAMS for `{id}`: {response}"
        );
    }
    daemon.stop().await;
}

#[tokio::test]
async fn a_deleted_sessions_id_is_free_again() {
    let dir = tempfile::tempdir().unwrap();
    let daemon = Daemon::start(dir.path(), "d", 0).await;
    let id = fresh_id();
    daemon
        .ok("session.create", serde_json::json!({"session_id": id}))
        .await;
    daemon
        .ok("session.delete", serde_json::json!({"session_id": id}))
        .await;
    let again = daemon
        .ok("session.create", serde_json::json!({"session_id": id}))
        .await;
    assert_eq!(again["existing"], false, "{again}");
    assert_eq!(daemon.listed(&id).await, 1);
    daemon.stop().await;
}

#[tokio::test]
async fn a_sidechains_id_is_taken() {
    let (srv, _seen) = crate::support::start_scripted_server(
        vec![],
        crate::support::ask_settings(),
        std::time::Duration::ZERO,
    )
    .await;
    let parent = call_frame(&srv.sock, "session.create", serde_json::json!({})).await;
    let parent = parent["result"]["session_id"].as_str().unwrap().to_string();
    let child = crate::support::inject_sidechain(&srv, &parent)
        .await
        .to_string();

    let refused = call_frame(
        &srv.sock,
        "session.create",
        serde_json::json!({"session_id": child}),
    )
    .await;
    assert_eq!(
        refused["error"]["code"], -32025,
        "SESSION_EXISTS: {refused}"
    );
    assert_eq!(
        refused["error"]["data"]["session_kind"], "sidechain",
        "{refused}"
    );
    srv.stop().await;
}

fn parsed_time(value: &serde_json::Value) -> time::OffsetDateTime {
    time::OffsetDateTime::parse(
        value.as_str().unwrap(),
        &time::format_description::well_known::Rfc3339,
    )
    .unwrap()
}

#[tokio::test]
async fn the_same_id_asked_for_with_other_options_is_refused_and_keeps_its_own() {
    let dir = tempfile::tempdir().unwrap();
    let daemon = Daemon::start(dir.path(), "d", 0).await;
    let id = fresh_id();
    let options = serde_json::json!({"permission_mode": "default", "attended": false});
    let first = daemon
        .ok(
            "session.create",
            serde_json::json!({"session_id": id, "options": options}),
        )
        .await;
    assert_eq!(first["existing"], false, "{first}");

    let retried = daemon
        .ok(
            "session.create",
            serde_json::json!({"session_id": id, "options": options}),
        )
        .await;
    assert_eq!(
        retried["existing"], true,
        "the same request again: {retried}"
    );
    assert_eq!(retried["created_at"], first["created_at"], "{retried}");

    let refused = daemon
        .call(
            "session.create",
            serde_json::json!({"session_id": id, "options": {
                "permission_mode": "plan", "attended": false,
                "output_schema": {"type": "object"}}}),
        )
        .await;
    assert_eq!(
        refused["error"]["code"], -32025,
        "SESSION_EXISTS: {refused}"
    );
    assert_eq!(
        refused["error"]["data"]["options_differ"],
        serde_json::json!(["output_schema", "permission_mode"]),
        "{refused}"
    );
    assert_eq!(refused["error"]["data"]["scene"], "code", "{refused}");

    let bare = daemon
        .call("session.create", serde_json::json!({"session_id": id}))
        .await;
    assert_eq!(
        bare["error"]["data"]["options_differ"],
        serde_json::json!(["attended", "permission_mode"]),
        "{bare}"
    );

    let again = daemon
        .ok(
            "session.create",
            serde_json::json!({"session_id": id, "options": options}),
        )
        .await;
    assert_eq!(
        again["existing"], true,
        "the refusals changed nothing: {again}"
    );
    daemon.stop().await;
}

#[tokio::test]
async fn after_a_restart_the_same_request_finds_the_session_created_when_it_was() {
    let dir = tempfile::tempdir().unwrap();
    let id = fresh_id();
    let options = serde_json::json!({"permission_mode": "plan", "auto_name": false});
    let before = Daemon::start(dir.path(), "before", 1).await;
    let first = before
        .ok(
            "session.create",
            serde_json::json!({"session_id": id, "options": options}),
        )
        .await;
    before
        .ok(
            "session.run_turn",
            serde_json::json!({"session_id": id, "message": "hi"}),
        )
        .await;
    before.stop().await;

    let restarted_at = time::OffsetDateTime::now_utc();
    let after = Daemon::start(dir.path(), "after", 0).await;
    let refused = after
        .call(
            "session.create",
            serde_json::json!({"session_id": id, "options": {"permission_mode": "plan"}}),
        )
        .await;
    assert_eq!(
        refused["error"]["data"]["options_differ"],
        serde_json::json!(["auto_name"]),
        "{refused}"
    );
    let again = after
        .ok(
            "session.create",
            serde_json::json!({"session_id": id, "options": options}),
        )
        .await;
    assert_eq!(again["existing"], true, "{again}");
    let created = parsed_time(&again["created_at"]);
    assert!(
        created < restarted_at,
        "created_at is when the session was created, not when it came back: {again}"
    );
    assert!(
        (created - parsed_time(&first["created_at"])).abs() < time::Duration::seconds(5),
        "{first} {again}"
    );
    let got = after
        .ok("session.get", serde_json::json!({"session_id": id}))
        .await;
    assert_eq!(got["created_at"], again["created_at"], "{got}");
    after.stop().await;
}
