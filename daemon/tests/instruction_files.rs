//! Which `AGENTS.md` files reach a session's context, seen in the
//! request a session's first turn sends: the user's global one, then every one from the
//! repository root down to the working directory, and nothing above the
//! repository.

use crate::support::text_round;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use daemon::config::{load_daemon_config, StaticDaemonPaths};
use daemon::DaemonServer;
use model::mock::MockAnthropicClient;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio_util::sync::CancellationToken;

struct Daemon {
    sock: PathBuf,
    model: Arc<MockAnthropicClient>,
    _dir: tempfile::TempDir,
}

impl Daemon {
    async fn start(prepare: impl FnOnce(&Path, &Path, &Path)) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let above = dir.path().join("above");
        let repo = above.join("repo");
        let work = repo.join("pkg");
        for d in [&home, &work] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        prepare(&home, &above, &repo);

        let sock = dir.path().join("instructions.sock");
        let paths =
            StaticDaemonPaths::with_project(home.clone(), work.clone()).with_global(home.clone());
        let mut config = load_daemon_config("claude-sonnet-4-6", 2000, Some(&sock), "code", &paths);
        config.settings.values.memory_enabled = false;
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
        std::fs::write(work.join("AGENTS.md"), "CWD-MARK").unwrap();
        Self {
            sock,
            model,
            _dir: dir,
        }
    }

    async fn call(&self, method: &str, params: Value) -> Value {
        let request = json!({"jsonrpc":"2.0","method":method,"params":params,"id":1});
        let mut conn = UnixStream::connect(&self.sock).await.unwrap();
        conn.write_all(format!("{request}\n").as_bytes())
            .await
            .unwrap();
        let mut reader = BufReader::new(conn);
        loop {
            let mut line = String::new();
            assert!(reader.read_line(&mut line).await.unwrap() > 0);
            let frame: Value = serde_json::from_str(&line).unwrap();
            if frame.get("id").is_some() {
                assert!(frame.get("result").is_some(), "{method} failed: {frame}");
                return frame["result"].clone();
            }
        }
    }

    async fn context_text(&self) -> String {
        self.model.push_turn(text_round("ok"));
        let session = self.call("session.create", json!({})).await["session_id"]
            .as_str()
            .unwrap()
            .to_string();
        self.call(
            "session.run_turn",
            json!({"session_id": session, "message": "hi"}),
        )
        .await;
        let sent = self.model.nth_request(0).expect("a call was made");
        serde_json::to_string(&sent).unwrap()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_context_has_the_global_and_repository_files_and_nothing_above_the_repository() {
    let daemon = Daemon::start(|home, above, repo| {
        std::fs::write(home.join("AGENTS.md"), "GLOBAL-MARK").unwrap();
        std::fs::write(above.join("AGENTS.md"), "ABOVE-REPO-MARK").unwrap();
        std::fs::write(repo.join("AGENTS.md"), "REPO-MARK").unwrap();
    })
    .await;
    let text = daemon.context_text().await;
    for mark in ["CWD-MARK", "REPO-MARK", "GLOBAL-MARK"] {
        assert!(text.contains(mark), "{mark} missing from the context");
    }
    assert!(
        !text.contains("ABOVE-REPO-MARK"),
        "a file above the repository reached the context"
    );
    assert!(
        text.find("GLOBAL-MARK") < text.find("REPO-MARK")
            && text.find("REPO-MARK") < text.find("CWD-MARK"),
        "global first, nearest last"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn over_the_size_cap_the_files_nearest_the_working_directory_stay() {
    let daemon = Daemon::start(|_, _, repo| {
        std::fs::write(repo.join("AGENTS.md"), "x".repeat(25_000) + "REPO-TAIL").unwrap();
    })
    .await;
    let text = daemon.context_text().await;
    assert!(text.contains("CWD-MARK"), "the nearest file was dropped");
}
