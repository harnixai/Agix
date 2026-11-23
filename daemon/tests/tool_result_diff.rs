//! `Write` / `Edit` report what they changed on the `tool_result` frame as
//! `file_changes` — hashes, sizes and a unified diff the host can apply —
//! while the text the model reads is what it always was; content too large
//! or not text carries hashes only, and a redacting transformer takes the
//! diff away.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use daemon::config::{load_daemon_config, StaticDaemonPaths};
use daemon::{DaemonServer, SessionCustomizer};
use model::mock::MockAnthropicClient;
use model::stream::StreamEvent;
use serde_json::{json, Value};

use crate::support::{text_round, tool_round};
use sha2::Digest;
use tokio_util::sync::CancellationToken;

fn read_round(file: &std::path::Path) -> Vec<StreamEvent> {
    tool_round(
        "Read",
        &format!("toolu_read_{}", file.file_name().unwrap().to_string_lossy()),
        json!({ "file_path": file }),
    )
}

struct Daemon {
    sock: PathBuf,
    cwd: PathBuf,
    model: Arc<MockAnthropicClient>,
    _dir: tempfile::TempDir,
}

impl Daemon {
    async fn start(model: Arc<MockAnthropicClient>, customize: Option<SessionCustomizer>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("diff.sock");
        let cwd = dir.path().join("work").canonicalize_or_create();
        let mut config = load_daemon_config(
            "claude-sonnet-4-6",
            None,
            Some(&sock),
            "code",
            &StaticDaemonPaths::new(dir.path().to_path_buf()),
        );
        config.settings.values.memory.enabled = false;
        config.settings.values.permissions.client_may_loosen = true;
        let pool = daemon::assemble::pool(
            &config,
            Arc::new(scene::code::spec(scene::code::Interaction::Interactive)),
            daemon::Assembly {
                cwd: Some(cwd.clone()),
                model_client: Some(model.clone()),
                customize_session: customize,
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
            cwd,
            model,
            _dir: dir,
        }
    }

    async fn ok(&self, method: &str, params: Value) -> Value {
        crate::support::call(&self.sock, method, params).await
    }

    async fn watch(&self, session_id: &str) -> Arc<Mutex<Vec<Value>>> {
        crate::support::watch(
            &self.sock,
            "session.subscribe",
            json!({"session_id": session_id}),
            |frame| (frame["method"] == "session.event").then(|| frame["params"]["event"].clone()),
        )
        .await
    }

    /// A session allowed to write without asking, run for one turn; its frames.
    async fn one_turn(&self) -> Vec<Value> {
        let session = self
            .ok(
                "session.create",
                json!({"options": {"settings": {"permissions": {
                    "mode": "default",
                    "allow": ["Write", "Edit", "Agent", "Bash"],
                }}}}),
            )
            .await["session_id"]
            .as_str()
            .unwrap()
            .to_string();
        let frames = self.watch(&session).await;
        self.ok(
            "session.run_turn",
            json!({"session_id": session, "message": "go"}),
        )
        .await;
        for _ in 0..400 {
            {
                let seen = frames.lock().unwrap();
                if seen.iter().any(|f| f["kind"] == "turn_complete") {
                    return seen.clone();
                }
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("turn_complete never arrived");
    }

    /// What the model was handed back for `tool_use_id`.
    fn answer_to(&self, tool_use_id: &str) -> String {
        for n in 0..self.model.calls() {
            let request = serde_json::to_value(self.model.nth_request(n).unwrap()).unwrap();
            for message in request["messages"].as_array().unwrap() {
                for block in message["content"].as_array().into_iter().flatten() {
                    if block["type"] == "tool_result" && block["tool_use_id"] == tool_use_id {
                        return match &block["content"] {
                            Value::String(s) => s.clone(),
                            blocks => blocks
                                .as_array()
                                .unwrap()
                                .iter()
                                .filter_map(|b| b["text"].as_str())
                                .collect(),
                        };
                    }
                }
            }
        }
        panic!("the model was never answered for {tool_use_id}");
    }
}

trait CanonicalOrCreate {
    fn canonicalize_or_create(self) -> PathBuf;
}

impl CanonicalOrCreate for PathBuf {
    fn canonicalize_or_create(self) -> PathBuf {
        std::fs::create_dir_all(&self).unwrap();
        self.canonicalize().unwrap()
    }
}

fn result_of<'a>(frames: &'a [Value], id: &str) -> &'a Value {
    frames
        .iter()
        .find(|f| f["kind"] == "tool_result" && f["id"] == id)
        .unwrap_or_else(|| panic!("no tool_result for {id}: {frames:#?}"))
}

fn sha256(content: &[u8]) -> String {
    format!("sha256:{}", hex::encode(sha2::Sha256::digest(content)))
}

/// Applies a unified diff the way `patch` does, so the test reads the diff
/// the way a host would rather than comparing it with another diff.
fn apply(before: &str, diff: &str) -> String {
    let old: Vec<&str> = before.split_inclusive('\n').collect();
    let mut out = String::new();
    let mut at = 0usize;
    let mut lines = diff.lines().skip(2).peekable();
    while let Some(header) = lines.next() {
        assert!(header.starts_with("@@ -"), "not a hunk header: {header}");
        let start: usize = header[4..]
            .split([',', ' '])
            .next()
            .unwrap()
            .parse()
            .unwrap();
        while at + 1 < start.max(1) {
            out.push_str(old[at]);
            at += 1;
        }
        while let Some(line) = lines.peek().copied() {
            if line.starts_with("@@") {
                break;
            }
            lines.next();
            let no_newline = lines.peek().is_some_and(|l| l.starts_with('\\'));
            if no_newline {
                lines.next();
            }
            let eol = if no_newline { "" } else { "\n" };
            match line.split_at(1) {
                (" ", body) => {
                    out.push_str(body);
                    out.push_str(eol);
                    at += 1;
                }
                ("-", _) => at += 1,
                ("+", body) => {
                    out.push_str(body);
                    out.push_str(eol);
                }
                _ => panic!("not a hunk line: {line:?}"),
            }
        }
    }
    for line in &old[at..] {
        out.push_str(line);
    }
    out
}

const LATIN1: &[u8] = b"caf\xe9 cr\xe8me\n";

const BEFORE: &str =
    "fn main() {\n    let a = 1;\n    let b = 2;\n    println!(\"{}\", a + b);\n}\n";

#[tokio::test]
async fn an_edit_reports_its_change_on_the_frame() {
    let model = Arc::new(MockAnthropicClient::new());
    let daemon = Daemon::start(model.clone(), None).await;
    let file = daemon.cwd.join("main.rs");
    std::fs::write(&file, BEFORE).unwrap();
    model.push_turn(read_round(&file));
    model.push_turn(tool_round(
        "Edit",
        "toolu_edit",
        json!({"file_path": file, "old_string": "let b = 2;", "new_string": "let b = 3;\n    let c = 4;"}),
    ));
    model.push_turn(text_round("edited"));

    let frames = daemon.one_turn().await;
    let after = std::fs::read_to_string(&file).unwrap();
    let frame = result_of(&frames, "toolu_edit");
    let changes = frame["file_changes"].as_array().expect("file_changes");
    assert_eq!(changes.len(), 1, "{frame}");
    let change = &changes[0];
    assert_eq!(change["path"], file.display().to_string());
    assert_eq!(change["op"], "modify");
    assert_eq!(change["before_hash"], sha256(BEFORE.as_bytes()));
    assert_eq!(change["after_hash"], sha256(after.as_bytes()));
    assert_eq!(change["before_bytes"], BEFORE.len());
    assert_eq!(change["after_bytes"], after.len());
    assert_eq!(
        (&change["added"], &change["removed"]),
        (&json!(2), &json!(1))
    );
    assert_eq!(change["truncated"], false);
    assert_eq!(change["omitted"], Value::Null);
    let diff = change["unified_diff"].as_str().unwrap();
    assert!(diff.starts_with("--- a/main.rs\n+++ b/main.rs\n"), "{diff}");
    assert_eq!(apply(BEFORE, diff), after);

    let shown = file.display();
    assert_eq!(
        daemon.answer_to("toolu_edit"),
        format!(
            "Applied 1 edit (1 replacement) to {shown} (71 → 86 bytes)\n\n\
             --- {shown} (before)\n+++ {shown} (after)\n\
             @@ -1,5 +1,6 @@\n fn main() {{\n     let a = 1;\n-    let b = 2;\n+    let b = 3;\n\
             +    let c = 4;\n     println!(\"{{}}\", a + b);\n }}\n"
        ),
        "the model's text is what it always was"
    );
    assert_eq!(frame["content"], daemon.answer_to("toolu_edit"));
}

#[tokio::test]
async fn a_write_reports_create_and_modify() {
    let model = Arc::new(MockAnthropicClient::new());
    let daemon = Daemon::start(model.clone(), None).await;
    let file = daemon.cwd.join("notes.txt");
    model.push_turn(tool_round(
        "Write",
        "toolu_create",
        json!({"file_path": file, "content": "first\n"}),
    ));
    model.push_turn(tool_round(
        "Write",
        "toolu_modify",
        json!({"file_path": file, "content": "second\n"}),
    ));
    model.push_turn(text_round("written"));

    let frames = daemon.one_turn().await;
    let created = &result_of(&frames, "toolu_create")["file_changes"][0];
    assert_eq!(created["op"], "create", "{created}");
    assert_eq!(created["before_hash"], Value::Null);
    assert_eq!(created["before_bytes"], Value::Null);
    assert_eq!(created["after_hash"], sha256(b"first\n"));
    let diff = created["unified_diff"].as_str().unwrap();
    assert!(
        diff.starts_with("--- /dev/null\n+++ b/notes.txt\n"),
        "{diff}"
    );
    assert_eq!(apply("", diff), "first\n");

    let modified = &result_of(&frames, "toolu_modify")["file_changes"][0];
    assert_eq!(modified["op"], "modify", "{modified}");
    assert_eq!(modified["before_hash"], sha256(b"first\n"));
    assert_eq!(modified["after_hash"], sha256(b"second\n"));
    assert_eq!(
        apply("first\n", modified["unified_diff"].as_str().unwrap()),
        "second\n"
    );
    assert_eq!(
        daemon.answer_to("toolu_modify"),
        format!("Wrote 7 bytes to {}", file.display())
    );
}

#[tokio::test]
async fn large_and_binary_changes_carry_hashes_only() {
    let model = Arc::new(MockAnthropicClient::new());
    let daemon = Daemon::start(model.clone(), None).await;
    let big = "0123456789abcdef\n".repeat(2 * 1024 * 1024 / 17 + 1);
    let binary = daemon.cwd.join("latin1.txt");
    std::fs::write(&binary, "plain\n").unwrap();
    let read_at = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_577_836_800);
    std::fs::File::options()
        .write(true)
        .open(&binary)
        .unwrap()
        .set_modified(read_at)
        .unwrap();
    let spread = |mark: &str| -> String {
        (0..3000)
            .map(|i| format!("{i}{mark}\n\n\n\n\n\n\n\n"))
            .collect()
    };
    let many = daemon.cwd.join("many.txt");
    std::fs::write(&many, spread("")).unwrap();
    model.push_turn(tool_round(
        "Write",
        "toolu_big",
        json!({"file_path": daemon.cwd.join("big.txt"), "content": big}),
    ));
    model.push_turn(read_round(&binary));
    model.push_turn(tool_round(
        "Bash",
        "toolu_latin1",
        json!({"command": format!(
            "printf 'caf\\351 cr\\350me\\n' > {0} && TZ=UTC touch -t 202001010000 {0}",
            binary.display()
        )}),
    ));
    model.push_turn(read_round(&many));
    model.push_turn(tool_round(
        "Write",
        "toolu_binary",
        json!({"file_path": binary, "content": "now text\n"}),
    ));
    model.push_turn(tool_round(
        "Write",
        "toolu_many",
        json!({"file_path": many, "content": spread("!")}),
    ));
    model.push_turn(text_round("done"));

    let frames = daemon.one_turn().await;
    let large = &result_of(&frames, "toolu_big")["file_changes"][0];
    assert_eq!(large["unified_diff"], Value::Null, "{large}");
    assert_eq!(large["omitted"], "too_large");
    assert_eq!(large["after_hash"], sha256(big.as_bytes()));

    let binary = &result_of(&frames, "toolu_binary")["file_changes"][0];
    assert_eq!(binary["unified_diff"], Value::Null, "{binary}");
    assert_eq!(binary["omitted"], "binary");
    assert_eq!(binary["before_hash"], sha256(LATIN1));
    assert_eq!(binary["after_hash"], sha256(b"now text\n"));

    let many = &result_of(&frames, "toolu_many")["file_changes"][0];
    assert_eq!(many["truncated"], true, "{many}");
    assert_eq!(
        (&many["added"], &many["removed"]),
        (&json!(3000), &json!(3000))
    );
    let diff = many["unified_diff"].as_str().unwrap();
    let hunks: Vec<&str> = diff.split("\n@@ ").skip(1).collect();
    assert!(!hunks.is_empty(), "{diff}");
    for hunk in &hunks {
        let (header, body) = hunk.split_once('\n').unwrap();
        let old_len: usize = header
            .trim_start_matches('-')
            .split(' ')
            .next()
            .and_then(|range| range.split(',').nth(1))
            .map_or(1, |n| n.parse().unwrap());
        let old_lines = body.lines().filter(|l| !l.starts_with('+')).count();
        assert_eq!(old_lines, old_len, "a hunk is cut short: @@ {header}");
    }
    let shown = diff
        .lines()
        .filter(|l| l.starts_with('+') && !l.starts_with("+++"))
        .count();
    assert!(shown < 3000, "{shown}");
}

#[tokio::test]
async fn a_redacting_transformer_strips_the_diff() {
    let model = Arc::new(MockAnthropicClient::new());
    let customize: SessionCustomizer = Arc::new(|builder, _| {
        builder.tool_result_transformer(Arc::new(
            base::interface::tool_result::RedactLiterals::new(["hunter2"]),
        ))
    });
    let daemon = Daemon::start(model.clone(), Some(customize)).await;
    let file = daemon.cwd.join("creds.txt");
    std::fs::write(&file, "password: changeme\n").unwrap();
    model.push_turn(tool_round(
        "Agent",
        "toolu_agent",
        json!({
            "description": "set the password",
            "prompt": "Set it",
            "subagent_type": "general-purpose",
        }),
    ));
    model.push_turn(read_round(&file));
    model.push_turn(tool_round(
        "Edit",
        "toolu_inner",
        json!({"file_path": file, "old_string": "changeme", "new_string": "hunter2"}),
    ));
    model.push_turn(text_round("set"));
    model.push_turn(read_round(&file));
    model.push_turn(tool_round(
        "Edit",
        "toolu_outer",
        json!({"file_path": file, "old_string": "hunter2", "new_string": "hunter2!"}),
    ));
    model.push_turn(text_round("done"));

    let frames = daemon.one_turn().await;
    let outer = &result_of(&frames, "toolu_outer")["file_changes"][0];
    assert_eq!(outer["unified_diff"], Value::Null, "{outer}");
    assert_eq!(outer["omitted"], "redacted");
    assert_eq!(outer["path"], file.display().to_string());
    assert_eq!(
        outer["after_hash"],
        sha256(std::fs::read(&file).unwrap().as_slice())
    );
    assert!(!outer.to_string().contains("hunter2"), "{outer}");

    let inner = frames
        .iter()
        .map(|f| &f["event"])
        .find(|e| e["kind"] == "tool_result" && e["id"] == "toolu_inner")
        .unwrap_or_else(|| panic!("no nested tool_result: {frames:#?}"));
    let nested = &inner["file_changes"][0];
    assert_eq!(nested["path"], file.display().to_string(), "{inner}");
    assert_eq!(nested["before_hash"], sha256(b"password: changeme\n"));
    assert!(nested["after_hash"].is_string(), "{nested}");
}
