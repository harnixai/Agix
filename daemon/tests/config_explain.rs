//! `config.explain` over the wire: for a key, every layer that went into its
//! value and what became of it there — a project value over a global one, a
//! `--set` over both, a looser project value an untrusted project may not
//! set, an `app` key no project file may set, the managed layer over a
//! looser file and under a stricter one, a session asking for a looser and a
//! tighter budget, and a key read only at start changed since.

use crate::support::*;
use daemon::config::{load_daemon_config, StaticDaemonPaths};
use daemon::DaemonServer;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

struct Daemon {
    sock: PathBuf,
    home: PathBuf,
    work: PathBuf,
    server: Arc<DaemonServer>,
    handle: tokio::task::JoinHandle<()>,
    _dir: tempfile::TempDir,
}

/// The daemon's state root, its project and the other projects of a test,
/// written before it starts.
struct Setup {
    global: Value,
    projects: Vec<(&'static str, Value)>,
    cli: Option<Value>,
    managed: Option<Value>,
}

impl Default for Setup {
    fn default() -> Self {
        Self {
            global: json!({}),
            projects: Vec::new(),
            cli: None,
            managed: None,
        }
    }
}

fn write(path: &Path, value: &Value) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, value.to_string()).unwrap();
}

impl Daemon {
    /// The daemon runs in `work`; `trusted` and `work` are trusted, every
    /// other project is not.
    async fn start(setup: Setup) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let (home, work) = (root.join("home"), root.join("work"));
        std::fs::create_dir_all(&work).unwrap();
        let mut global = setup.global;
        global["app"]["trusted_projects"] = json!([work, root.join("trusted")]);
        global["app"]["trust_all_projects"] = json!(false);
        write(&home.join("settings.json"), &global);
        for (name, settings) in &setup.projects {
            write(&root.join(name).join(".agix/settings.json"), settings);
        }
        let sock = root.join("explain.sock");
        let paths =
            StaticDaemonPaths::with_project(home.clone(), work.clone()).with_global(home.clone());
        let mut config =
            load_daemon_config("claude-sonnet-4-6", setup.cli, Some(&sock), "code", &paths);
        if let Some(managed) = &setup.managed {
            let path = root.join("it/managed-settings.json");
            write(&path, managed);
            config.managed_settings_path = Some(path);
        }
        let (client, _) = ScriptedClient::new(vec![text_round("done")]);
        let pool = daemon::assemble::pool(
            &config,
            Arc::new(scene::code::spec(scene::code::Interaction::Interactive)),
            daemon::Assembly {
                cwd: Some(work.clone()),
                model_client: Some(client),
                transcripts: daemon::Transcripts::Nowhere,
                ..Default::default()
            },
        )
        .await
        .expect("the daemon assembles");
        let server = Arc::new(DaemonServer::new(pool, CancellationToken::new()));
        let (serving, bind) = (server.clone(), sock.clone());
        let handle = tokio::spawn(async move {
            let _ = serving.serve_unix(&bind).await;
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
            home,
            work,
            server,
            handle,
            _dir: dir,
        }
    }

    fn project(&self, name: &str) -> PathBuf {
        self.work.parent().unwrap().join(name)
    }

    async fn stop(self) {
        self.server.shutdown_token().cancel();
        let _ = self.handle.await;
    }

    /// The explanation of the one key `params` names.
    async fn explain(&self, params: Value) -> Value {
        let result = call(&self.sock, "config.explain", params).await;
        let keys = result["keys"].as_array().unwrap();
        assert_eq!(keys.len(), 1, "{result}");
        keys[0].clone()
    }
}

/// Each layer's tier and outcome, lowest first, leaving out those that do
/// not set the key.
fn outcomes(explained: &Value) -> Vec<(String, String)> {
    explained["layers"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|l| l["outcome"] != "not_set")
        .map(|l| {
            (
                l["tier"].as_str().unwrap().to_string(),
                l["outcome"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

fn pairs(expected: &[(&str, &str)]) -> Vec<(String, String)> {
    expected
        .iter()
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect()
}

fn layer<'a>(explained: &'a Value, tier: &str) -> &'a Value {
    explained["layers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|l| l["tier"] == tier && l["outcome"] != "not_set")
        .unwrap_or_else(|| panic!("no {tier} layer sets it: {explained}"))
}

#[tokio::test]
async fn a_project_value_overrides_the_global_one_and_a_set_value_both() {
    let d = Daemon::start(Setup {
        global: json!({"turn": {"max_model_calls": 7, "parallel_tools": 4}}),
        projects: vec![("work", json!({"turn": {"max_model_calls": 3}}))],
        cli: Some(json!({"turn": {"parallel_tools": 2}})),
        ..Default::default()
    })
    .await;

    let explained = d.explain(json!({"key": "turn.max_model_calls"})).await;
    assert_eq!(explained["value"], 3, "{explained}");
    assert_eq!(explained["scope"], "session");
    assert_eq!(explained["read_at_start"], false);
    assert_eq!(
        outcomes(&explained),
        pairs(&[
            ("default", "overridden"),
            ("global", "overridden"),
            ("project", "applied")
        ]),
        "{explained}"
    );
    assert_eq!(layer(&explained, "global")["value"], 7);
    let origin = layer(&explained, "project")["origin"].as_str().unwrap();
    assert!(origin.ends_with("work/.agix/settings.json"), "{origin}");

    let explained = d.explain(json!({"key": "turn.parallel_tools"})).await;
    assert_eq!(explained["value"], 2, "{explained}");
    assert_eq!(
        outcomes(&explained),
        pairs(&[
            ("default", "overridden"),
            ("global", "overridden"),
            ("cli", "applied")
        ]),
        "{explained}"
    );

    let block = call(&d.sock, "config.explain", json!({"key": "turn"})).await;
    let keys: Vec<&str> = block["keys"]
        .as_array()
        .unwrap()
        .iter()
        .map(|k| k["key"].as_str().unwrap())
        .collect();
    assert!(
        keys.contains(&"turn.max_model_calls") && keys.contains(&"turn.parallel_tools"),
        "{block}"
    );
    assert_eq!(block["truncated"], false);
    d.stop().await;
}

#[tokio::test]
async fn an_untrusted_project_is_withheld_and_an_app_key_in_a_project_is_dropped() {
    let d = Daemon::start(Setup {
        projects: vec![
            ("other", json!({"sandbox": {"enabled": false}})),
            ("work", json!({"app": {"session_cap": 3}})),
        ],
        ..Default::default()
    })
    .await;
    let other = d.project("other");

    let explained = d
        .explain(json!({"key": "sandbox.enabled", "project_root": other}))
        .await;
    assert_eq!(explained["value"], true, "{explained}");
    assert_eq!(
        outcomes(&explained),
        pairs(&[("default", "applied"), ("project", "withheld_untrusted")]),
        "{explained}"
    );
    assert_eq!(layer(&explained, "project")["value"], false);

    let explained = d.explain(json!({"key": "app.session_cap"})).await;
    assert_eq!(explained["scope"], "process", "{explained}");
    assert_eq!(explained["read_at_start"], true);
    assert_eq!(
        outcomes(&explained),
        pairs(&[("default", "applied"), ("project", "dropped_not_global")]),
        "{explained}"
    );
    d.stop().await;
}

#[tokio::test]
async fn the_managed_layer_holds_over_a_looser_file_and_under_a_stricter_one() {
    let d = Daemon::start(Setup {
        global: json!({"turn": {"token_budget": 5000}}),
        projects: vec![("trusted", json!({"turn": {"token_budget": 500}}))],
        managed: Some(json!({"turn": {"token_budget": 1000}})),
        ..Default::default()
    })
    .await;

    let explained = d.explain(json!({"key": "turn.token_budget"})).await;
    assert_eq!(explained["value"], 1000, "{explained}");
    assert_eq!(
        outcomes(&explained),
        pairs(&[("global", "managed_overrode")]),
        "{explained}"
    );
    assert_eq!(explained["managed"]["value"], 1000);
    assert_eq!(explained["managed"]["outcome"], "applied");

    let explained = d
        .explain(json!({"key": "turn.token_budget", "project_root": d.project("trusted")}))
        .await;
    assert_eq!(explained["value"], 500, "{explained}");
    assert_eq!(
        outcomes(&explained),
        pairs(&[("global", "overridden"), ("project", "applied")]),
        "{explained}"
    );
    assert_eq!(explained["managed"]["outcome"], "held_tighter_below");
    d.stop().await;
}

#[tokio::test]
async fn a_session_asking_for_a_looser_budget_is_clamped_and_a_tighter_one_applies() {
    let d = Daemon::start(Setup {
        global: json!({"turn": {"token_budget": 1000}}),
        ..Default::default()
    })
    .await;
    let session = |budget: u64| {
        let sock = d.sock.clone();
        async move {
            let created = call(
                &sock,
                "session.create",
                json!({"options": {"settings": {"turn": {"token_budget": budget}}}}),
            )
            .await;
            created["session_id"].as_str().unwrap().to_string()
        }
    };

    let looser = session(5000).await;
    let explained = d
        .explain(json!({"key": "turn.token_budget", "session_id": looser}))
        .await;
    assert_eq!(explained["value"], 1000, "{explained}");
    assert_eq!(
        explained["session"],
        json!({"requested": 5000, "applied": 1000, "outcome": "clamped"})
    );
    assert_eq!(layer(&explained, "global")["outcome"], "applied");

    let tighter = session(200).await;
    let explained = d
        .explain(json!({"key": "turn.token_budget", "session_id": tighter}))
        .await;
    assert_eq!(explained["value"], 200, "{explained}");
    assert_eq!(
        explained["session"],
        json!({"requested": 200, "applied": 200, "outcome": "applied"})
    );

    let both = call_frame(
        &d.sock,
        "config.explain",
        json!({"key": "turn.token_budget", "session_id": tighter, "scene": "code"}),
    )
    .await;
    assert_eq!(
        both["error"]["code"],
        daemon::rpc::codes::INVALID_PARAMS,
        "{both}"
    );
    d.stop().await;
}

#[tokio::test]
async fn a_key_read_at_start_changed_since_is_pending_a_restart() {
    let d = Daemon::start(Setup {
        global: json!({"app": {"session_cap": 8}}),
        ..Default::default()
    })
    .await;

    let explained = d.explain(json!({"key": "app.session_cap"})).await;
    assert_eq!(explained["value"], 8, "{explained}");
    assert_eq!(explained["restart_pending"], false);

    let path = d.home.join("settings.json");
    let mut global: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    global["app"]["session_cap"] = json!(9);
    write(&path, &global);
    let explained = d.explain(json!({"key": "app.session_cap"})).await;
    assert_eq!(explained["value"], 9, "{explained}");
    assert_eq!(explained["restart_pending"], true);
    assert_eq!(
        outcomes(&explained),
        pairs(&[("default", "overridden"), ("global", "applied")]),
        "{explained}"
    );
    d.stop().await;
}

#[tokio::test]
async fn a_key_no_setting_reads_is_refused_with_its_new_name_when_renamed() {
    let d = Daemon::start(Setup::default()).await;
    for (key, says) in [
        ("turn.max_model_calz", "no setting reads"),
        ("permission_mode", "permissions.mode"),
    ] {
        let refused = call_frame(&d.sock, "config.explain", json!({"key": key})).await;
        assert_eq!(
            refused["error"]["code"],
            daemon::rpc::codes::INVALID_PARAMS,
            "{refused}"
        );
        let message = refused["error"]["message"].as_str().unwrap();
        assert!(message.contains(says), "{message}");
    }
    d.stop().await;
}

#[tokio::test]
async fn a_credential_is_redacted_unless_secrets_are_asked_for() {
    let d = Daemon::start(Setup {
        global: json!({"mcp": {"servers": {"gh": {
            "command": "/nonexistent/mcp-server",
            "env": {"GH_TOKEN": "ghp_supersecretvalue"},
        }}}}),
        ..Default::default()
    })
    .await;
    let key = "mcp.servers.gh.env.GH_TOKEN";

    let explained = d.explain(json!({"key": key})).await;
    let shown = explained.to_string();
    assert!(!shown.contains("supersecret"), "{shown}");
    assert_eq!(layer(&explained, "global")["outcome"], "applied");

    let explained = d
        .explain(json!({"key": key, "include_secrets": true}))
        .await;
    assert_eq!(explained["value"], "ghp_supersecretvalue", "{explained}");
    d.stop().await;
}

#[tokio::test]
async fn the_env_of_a_server_named_sandbox_is_redacted_too() {
    let d = Daemon::start(Setup {
        global: json!({"mcp": {"servers": {"sandbox": {
            "command": "/nonexistent/mcp-server",
            "env": {"GITHUB_PAT": "ghp_supersecretvalue"},
        }}}}),
        ..Default::default()
    })
    .await;

    for key in [
        "mcp.servers.sandbox.env",
        "mcp.servers.sandbox.env.GITHUB_PAT",
    ] {
        let explained = d.explain(json!({"key": key})).await;
        let shown = explained.to_string();
        assert!(!shown.contains("supersecret"), "{shown}");
    }
    let block = call(&d.sock, "config.explain", json!({"key": "mcp"})).await;
    assert!(!block.to_string().contains("supersecret"), "{block}");
    let tier = call(
        &d.sock,
        "config.get",
        json!({"tier": "global", "scene": "code"}),
    )
    .await;
    assert!(!tier.to_string().contains("supersecret"), "{tier}");
    d.stop().await;
}

#[tokio::test]
async fn headers_or_env_written_as_one_string_are_redacted() {
    let d = Daemon::start(Setup {
        global: json!({"mcp": {"servers": {
            "gh": {
                "url": "https://mcp.example.com",
                "headers": "Authorization: Bearer ghp_supersecretvalue",
            },
            "local": {
                "command": "/nonexistent/mcp-server",
                "env": "GITHUB_PAT=ghp_supersecretvalue",
            },
        }}}),
        ..Default::default()
    })
    .await;

    let block = call(&d.sock, "config.explain", json!({"key": "mcp"})).await;
    assert!(!block.to_string().contains("supersecret"), "{block}");
    let tier = call(
        &d.sock,
        "config.get",
        json!({"tier": "global", "scene": "code"}),
    )
    .await;
    assert!(!tier.to_string().contains("supersecret"), "{tier}");
    d.stop().await;
}

#[tokio::test]
async fn a_bypass_the_managed_layer_disables_is_not_reported_applied() {
    let d = Daemon::start(Setup {
        projects: vec![(
            "trusted",
            json!({"permissions": {"mode": "bypassPermissions"}}),
        )],
        managed: Some(json!({"permissions": {"disable_bypass": true}})),
        ..Default::default()
    })
    .await;

    let explained = d
        .explain(json!({"key": "permissions.mode", "project_root": d.project("trusted")}))
        .await;
    assert_eq!(explained["value"], "default", "{explained}");
    let project = layer(&explained, "project");
    assert_eq!(project["value"], "bypassPermissions", "{explained}");
    assert_eq!(project["outcome"], "managed_overrode", "{explained}");
    d.stop().await;
}

#[tokio::test]
async fn a_mode_the_host_switched_to_since_is_overridden_not_clamped() {
    let d = Daemon::start(Setup {
        global: json!({"permissions": {"mode": "acceptEdits"}}),
        ..Default::default()
    })
    .await;
    let created = call(
        &d.sock,
        "session.create",
        json!({"options": {"settings": {"permissions": {"mode": "default"}}}}),
    )
    .await;
    let sid = created["session_id"].as_str().unwrap().to_string();
    let switched = call(
        &d.sock,
        "session.setPermissionMode",
        json!({"session_id": sid, "mode": "acceptEdits"}),
    )
    .await;
    assert_eq!(switched["mode"], "acceptEdits", "{switched}");

    let explained = d
        .explain(json!({"key": "permissions.mode", "session_id": sid}))
        .await;
    assert_eq!(explained["value"], "acceptEdits", "{explained}");
    assert_eq!(
        explained["session"],
        json!({"requested": "default", "applied": "acceptEdits", "outcome": "overridden"}),
        "{explained}"
    );
    d.stop().await;
}

#[tokio::test]
async fn why_a_credential_does_not_fit_is_said_without_it() {
    let proxy = "http://u:ghp_supersecretvalue@proxy:3128";
    let d = Daemon::start(Setup {
        global: json!({"sandbox": {"network": {"mode": "proxy", "proxy": proxy}}}),
        ..Default::default()
    })
    .await;
    let key = "sandbox.network.proxy";

    let explained = d.explain(json!({"key": key})).await;
    let global = layer(&explained, "global");
    assert_eq!(global["outcome"], "ignored", "{explained}");
    let error = global["error"].as_str().unwrap_or_default();
    assert!(error.contains(key), "{error}");
    assert!(
        !explained.to_string().contains("supersecret"),
        "{explained}"
    );

    let shown = d
        .explain(json!({"key": key, "include_secrets": true}))
        .await;
    let error = layer(&shown, "global")["error"]
        .as_str()
        .unwrap_or_default();
    assert!(error.contains("supersecret"), "{shown}");

    let doctor = call(&d.sock, "daemon.doctor", json!({})).await;
    assert!(doctor.to_string().contains(key), "{doctor}");
    assert!(!doctor.to_string().contains("supersecret"), "{doctor}");
    d.stop().await;
}

#[tokio::test]
async fn the_managed_layer_is_shown_redacted() {
    let d = Daemon::start(Setup {
        managed: Some(json!({"mcp": {"servers": {"gh": {
            "command": "/nonexistent/mcp-server",
            "env": {"GH_TOKEN": "ghp_supersecretvalue"},
        }}}})),
        ..Default::default()
    })
    .await;
    let effective = call(
        &d.sock,
        "config.get",
        json!({"tier": "effective", "scene": "code"}),
    )
    .await;
    assert!(effective["managed"]["mcp"].is_object(), "{effective}");
    assert!(
        !effective.to_string().contains("supersecret"),
        "{effective}"
    );
    let shown = call(
        &d.sock,
        "config.get",
        json!({"tier": "effective", "scene": "code", "include_secrets": true}),
    )
    .await;
    assert!(shown.to_string().contains("supersecret"), "{shown}");
    let doctor = call(&d.sock, "daemon.doctor", json!({})).await;
    assert!(!doctor.to_string().contains("supersecret"), "{doctor}");
    d.stop().await;
}

#[tokio::test]
async fn why_a_managed_layer_cannot_be_honoured_is_said_without_its_credential() {
    let d = Daemon::start(Setup {
        managed: Some(json!({"sandbox": {"network": {
            "mode": "proxy",
            "proxy": "http://u:ghp_supersecretvalue@proxy:3128",
        }}})),
        ..Default::default()
    })
    .await;
    let effective = call(
        &d.sock,
        "config.get",
        json!({"tier": "effective", "scene": "code"}),
    )
    .await;
    let error = effective["managed"]["error"].as_str().unwrap_or_default();
    assert!(error.contains("sandbox.network.proxy"), "{effective}");
    assert!(
        !effective.to_string().contains("supersecret"),
        "{effective}"
    );
    let doctor = call(&d.sock, "daemon.doctor", json!({})).await;
    assert!(!doctor.to_string().contains("supersecret"), "{doctor}");
    d.stop().await;
}
