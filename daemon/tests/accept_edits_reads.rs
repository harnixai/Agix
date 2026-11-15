//! What an `acceptEdits` session reads without asking, seen from a host: the
//! project, and nothing outside it that `sandbox.read_directories` does not
//! name — the same as `default`.

use crate::support::{text_round, tool_round};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use daemon::config::{load_daemon_config, StaticDaemonPaths};
use daemon::DaemonServer;
use model::mock::MockAnthropicClient;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

struct Daemon {
    sock: PathBuf,
    work: PathBuf,
    /// Outside the project and every directory the instance reads anyway — a
    /// stand-in for `~/Documents`.
    elsewhere: PathBuf,
    model: Arc<MockAnthropicClient>,
    _dir: tempfile::TempDir,
}

impl Daemon {
    async fn start(read_directories: &[&str]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let (home, work, elsewhere) = (
            dir.path().join("home"),
            dir.path().join("work"),
            dir.path().join("elsewhere"),
        );
        for d in [&home, &work, &elsewhere] {
            std::fs::create_dir_all(d).unwrap();
        }
        let listed: Vec<PathBuf> = read_directories
            .iter()
            .map(|d| dir.path().join(d))
            .collect();
        let global = json!({ "sandbox": {
            "dangerously_disable_sandbox": true,
            "read_directories": listed,
        }});
        std::fs::write(home.join("settings.json"), global.to_string()).unwrap();

        let sock = dir.path().join("accept-edits.sock");
        let paths =
            StaticDaemonPaths::with_project(home.clone(), work.clone()).with_global(home.clone());
        let mut config = load_daemon_config("claude-sonnet-4-6", 2000, Some(&sock), "code", &paths);
        config.settings.values.memory_enabled = false;
        config.settings.values.permission_mode = base::permission::PermissionMode::AcceptEdits;

        let model = Arc::new(MockAnthropicClient::new());
        let pool = daemon::assemble::pool(
            &config,
            Arc::new(scene::code::spec(scene::code::Interaction::Interactive)),
            daemon::Assembly {
                cwd: Some(work.clone()),
                model_client: Some(model.clone()),
                transcripts: daemon::Transcripts::Nowhere,
                ..Default::default()
            },
        )
        .await
        .expect("the daemon assembles");

        let server = Arc::new(DaemonServer::new(pool, CancellationToken::new()));
        let listener = DaemonServer::bind_unix(&sock).unwrap();
        tokio::spawn(async move {
            let _ = server.serve_unix_listener(listener).await;
        });
        Self {
            sock,
            work,
            elsewhere,
            model,
            _dir: dir,
        }
    }

    /// Runs a turn, denying every permission prompt it raises; returns the
    /// tool calls the prompts were about.
    async fn turn_denying_prompts(&self) -> Vec<String> {
        let mut client = crate::support::begin(
            &self.sock,
            "session.run_turn",
            json!({"message":"read the notes"}),
        )
        .await;
        let mut asked = Vec::new();
        loop {
            let frame = tokio::time::timeout(Duration::from_secs(120), client.next_frame())
                .await
                .expect("the turn ended")
                .unwrap()
                .expect("connection closed before the turn ended");
            if frame.get("id").is_some() {
                assert!(frame.get("result").is_some(), "turn failed: {frame}");
                return asked;
            }
            let event = &frame["params"]["event"];
            if event["kind"] == "prompt" {
                assert_eq!(event["prompt_type"], "permission", "{event}");
                asked.push(event["tool_use_id"].as_str().unwrap_or("").to_string());
                deny(
                    &self.sock,
                    frame["params"]["session_id"].as_str().unwrap(),
                    event["prompt_id"].as_str().unwrap(),
                )
                .await;
            }
        }
    }
}

async fn deny(sock: &Path, session_id: &str, prompt_id: &str) {
    let reply = crate::support::call_frame(
        sock,
        "session.respondToPrompt",
        json!({
        "session_id": session_id, "prompt_id": prompt_id,
        "decision": {"type": "deny", "reason": "outside the project"}}),
    )
    .await;
    assert_eq!(reply["result"]["prompt_id"], prompt_id, "{reply}");
}

fn read(path: &Path) -> Value {
    json!({ "file_path": path.display().to_string() })
}

#[tokio::test(flavor = "multi_thread")]
async fn reading_outside_the_project_is_asked_about_and_inside_it_is_not() {
    let daemon = Daemon::start(&[]).await;
    let inside = daemon.work.join("notes.txt");
    let outside = daemon.elsewhere.join("notes.txt");
    std::fs::write(&inside, "inside-the-project").unwrap();
    std::fs::write(&outside, "outside-the-project").unwrap();

    daemon
        .model
        .push_turn(tool_round("Read", "toolu_inside", read(&inside)));
    daemon
        .model
        .push_turn(tool_round("Read", "toolu_outside", read(&outside)));
    daemon.model.push_turn(tool_round(
        "Bash",
        "toolu_cat",
        json!({ "command": format!("cat {}", outside.display()) }),
    ));
    daemon.model.push_turn(text_round("done"));

    let asked = daemon.turn_denying_prompts().await;
    assert_eq!(asked, ["toolu_outside", "toolu_cat"]);
    let shown = serde_json::to_string(&daemon.model.nth_request(1).unwrap()).unwrap();
    assert!(
        shown.contains("inside-the-project"),
        "the project file was not read: {shown}"
    );
    let last = serde_json::to_string(&daemon.model.nth_request(3).unwrap()).unwrap();
    assert!(
        !last.contains("outside-the-project"),
        "a denied read reached the model: {last}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_listed_read_directory_is_read_without_asking() {
    let daemon = Daemon::start(&["elsewhere"]).await;
    let outside = daemon.elsewhere.join("notes.txt");
    std::fs::write(&outside, "listed-directory").unwrap();

    daemon
        .model
        .push_turn(tool_round("Read", "toolu_listed", read(&outside)));
    daemon.model.push_turn(text_round("done"));

    let asked = daemon.turn_denying_prompts().await;
    assert!(asked.is_empty(), "{asked:?}");
    let shown = serde_json::to_string(&daemon.model.nth_request(1).unwrap()).unwrap();
    assert!(shown.contains("listed-directory"), "{shown}");
}
