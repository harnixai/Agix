//! A `Read(...)` deny rule keeps a file out of what `Grep` and `Glob` return,
//! not only out of `Read`: searching the directory above it does not walk
//! past the rule.

use crate::support::{text_round, tool_round};
use std::path::PathBuf;
use std::sync::Arc;

use daemon::config::{load_daemon_config, StaticDaemonPaths};
use daemon::DaemonServer;
use model::mock::MockAnthropicClient;
use serde_json::json;
use tokio_util::sync::CancellationToken;

struct Daemon {
    sock: PathBuf,
    work: PathBuf,
    model: Arc<MockAnthropicClient>,
    _dir: tempfile::TempDir,
}

impl Daemon {
    async fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let (home, work) = (dir.path().join("home"), dir.path().join("work"));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(work.join(".agix")).unwrap();
        std::fs::write(
            work.join(".agix/settings.json"),
            json!({"permissions": {"deny": ["Read(secret/**)"]}}).to_string(),
        )
        .unwrap();

        let sock = dir.path().join("read-deny.sock");
        let paths =
            StaticDaemonPaths::with_project(home.clone(), work.clone()).with_global(home.clone());
        let mut config = load_daemon_config("claude-sonnet-4-6", 2000, Some(&sock), "code", &paths);
        config.settings.values.memory_enabled = false;
        config.settings.values.permissions.mode = base::permission::PermissionMode::AcceptEdits;
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
            model,
            _dir: dir,
        }
    }

    async fn turn(&self) {
        crate::support::call(&self.sock, "session.run_turn", json!({"message":"search"})).await;
    }

    fn request(&self, n: usize) -> String {
        serde_json::to_string(&self.model.nth_request(n).unwrap()).unwrap()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn grep_and_glob_from_the_project_root_leave_out_what_a_read_deny_covers() {
    let daemon = Daemon::start().await;
    std::fs::create_dir_all(daemon.work.join("secret")).unwrap();
    std::fs::write(daemon.work.join("secret/k.txt"), "TOKEN-IN-SECRET\n").unwrap();
    std::fs::write(daemon.work.join("open.txt"), "TOKEN-IN-OPEN\n").unwrap();

    daemon.model.push_turn(tool_round(
        "Grep",
        "toolu_grep",
        json!({"pattern": "TOKEN-IN", "output_mode": "content"}),
    ));
    daemon.model.push_turn(tool_round(
        "Glob",
        "toolu_glob",
        json!({"pattern": "**/*.txt"}),
    ));
    daemon.model.push_turn(text_round("done"));
    daemon.turn().await;

    let after_grep = daemon.request(1);
    assert!(after_grep.contains("TOKEN-IN-OPEN"), "{after_grep}");
    assert!(
        !after_grep.contains("TOKEN-IN-SECRET"),
        "Grep read a file a Read deny covers: {after_grep}"
    );
    let after_glob = daemon.request(2);
    assert!(after_glob.contains("open.txt"), "{after_glob}");
    assert!(
        !after_glob.contains("k.txt"),
        "Glob listed a file a Read deny covers: {after_glob}"
    );
}
