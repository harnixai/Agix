//! A project whose `.agix` is a symlink to the global configuration
//! directory is still a project of its own: its repository's skills, hooks
//! and git wait for trust like any other, rather than passing as "no
//! project tier".

use crate::support::{text_round, tool_round};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use daemon::config::{load_daemon_config, StaticDaemonPaths};
use daemon::DaemonServer;
use model::client::{AnthropicClient, CountFuture, EventStream};
use model::stream::StreamEvent;
use model::types::MessagesRequest;
use serde_json::json;
use tokio_util::sync::CancellationToken;

struct Scripted(StdMutex<VecDeque<Vec<StreamEvent>>>);

impl AnthropicClient for Scripted {
    fn stream_messages(&self, _req: MessagesRequest) -> EventStream {
        let events = self
            .0
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| text_round("done"));
        Box::pin(futures::stream::iter(events.into_iter().map(Ok)))
    }

    fn count_tokens<'a>(&'a self, _req: &'a MessagesRequest) -> CountFuture<'a> {
        Box::pin(async { Ok(0usize) })
    }
}

struct Daemon {
    sock: PathBuf,
    repo: PathBuf,
    _dir: tempfile::TempDir,
    _handle: tokio::task::JoinHandle<()>,
}

impl Daemon {
    /// `~/.agix` is the global tier; `~/code/repo/.agix` links to it, and the
    /// repository ships a skill that grants `python3`.
    async fn start(global: serde_json::Value) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let global_dir = home.join(".agix");
        let repo = home.join("code").join("repo");
        std::fs::create_dir_all(&global_dir).unwrap();
        std::fs::create_dir_all(repo.join(".agents/skills/py")).unwrap();
        std::fs::write(global_dir.join("settings.json"), global.to_string()).unwrap();
        std::os::unix::fs::symlink("../../.agix", repo.join(".agix")).unwrap();
        std::fs::write(
            repo.join(".agents/skills/py/SKILL.md"),
            "---\nname: py\ndescription: run python\nallowed-tools: [\"Bash(python3:*)\"]\n---\nRun python.\n",
        )
        .unwrap();

        let sock = dir.path().join("symlink.sock");
        let paths = StaticDaemonPaths::with_project(global_dir.clone(), repo.clone())
            .with_global(global_dir.clone());
        let mut config = load_daemon_config("claude-sonnet-4-6", 2000, Some(&sock), "code", &paths);
        config.settings.values.memory.enabled = false;
        let rounds = VecDeque::from([
            tool_round("Skill", "skill-1", json!({"skill": "py"})),
            tool_round("Bash", "bash-1", json!({"command": "python3 -c 1"})),
            text_round("done"),
        ]);
        let pool = daemon::assemble::pool(
            &config,
            Arc::new(scene::code::spec(scene::code::Interaction::Interactive)),
            daemon::Assembly {
                cwd: Some(repo.clone()),
                permission_prompt_timeout: Some(Duration::from_secs(30)),
                model_client: Some(
                    Arc::new(Scripted(StdMutex::new(rounds))) as Arc<dyn AnthropicClient>
                ),
                transcripts: daemon::Transcripts::Nowhere,
                ..Default::default()
            },
        )
        .await
        .expect("the daemon assembles");
        let server = Arc::new(DaemonServer::new(pool, CancellationToken::new()));
        let bind = sock.clone();
        let handle = tokio::spawn(async move {
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
            repo,
            _dir: dir,
            _handle: handle,
        }
    }

    async fn call(&self, method: &str, params: serde_json::Value) -> serde_json::Value {
        crate::support::call(&self.sock, method, params).await
    }

    /// Run a turn on `sid`, refusing every prompt, and return the tools that
    /// asked.
    async fn turn_refusing_prompts(&self, sid: &str) -> Vec<String> {
        let mut client = crate::support::begin(
            &self.sock,
            "session.run_turn",
            json!({"session_id": sid, "message": "use the py skill"}),
        )
        .await;
        let mut asked = Vec::new();
        loop {
            let v = client.next_frame().await.unwrap().expect("turn: EOF");
            if v.get("id").is_some() {
                assert!(v.get("result").is_some(), "the turn failed: {v}");
                return asked;
            }
            let event = &v["params"]["event"];
            if event["kind"] == "prompt" {
                asked.push(event["tool_name"].as_str().unwrap_or_default().to_string());
                let prompt_id = event["prompt_id"].as_str().unwrap();
                self.call(
                    "session.respondToPrompt",
                    json!({"session_id": sid, "prompt_id": prompt_id,
                        "decision": {"type": "deny", "reason": "no"}}),
                )
                .await;
            }
        }
    }
}

#[tokio::test]
async fn a_project_agix_linked_to_the_global_directory_is_still_untrusted() {
    let daemon = Daemon::start(json!({"app": {"trust_all_projects": false}})).await;

    let created = daemon
        .call("session.create", json!({"project_root": daemon.repo}))
        .await;
    assert_eq!(created["project_trust"], "untrusted", "{created}");
    let status = daemon
        .call("project.status", json!({"project_root": daemon.repo}))
        .await;
    assert_eq!(status["trust"], "untrusted", "{status}");
    assert!(
        status["requires_trust"]
            .as_array()
            .unwrap()
            .iter()
            .any(|k| k == "skills/py"),
        "{status}"
    );

    let sid = created["session_id"].as_str().unwrap();
    let asked = daemon.turn_refusing_prompts(sid).await;
    assert!(
        asked.iter().any(|t| t == "Bash"),
        "python3 ran without asking after the repository's skill granted it: {asked:?}"
    );
}
