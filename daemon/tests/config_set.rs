//! `config.set` over the wire: a key written to the tier it names and read
//! back, removed with `null`, credentials kept in `settings.local.json`,
//! every key refused or none written, `dry_run`, an untrusted project's
//! limits, `if_revision` and the directory lock.

use crate::support::*;
use daemon::rpc::codes;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

fn read(path: &Path) -> Value {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or(Value::Null)
}

fn bytes(paths: &[PathBuf]) -> Vec<Option<Vec<u8>>> {
    paths.iter().map(|p| std::fs::read(p).ok()).collect()
}

async fn effective(sock: &Path, params: Value) -> Value {
    let mut params = params;
    params["tier"] = json!("effective");
    call(sock, "config.get", params).await["settings"].clone()
}

#[tokio::test]
async fn a_key_written_to_the_global_tier_reads_back_and_takes_effect() {
    let (server, sock, dir, handle) = start_server().await;
    let written = call(
        &sock,
        "config.set",
        json!({"tier": "global", "changes": {
            "permissions.mode": "acceptEdits",
            "sandbox.allowed_domains": ["github.com"]
        }}),
    )
    .await;
    assert_eq!(written["tier"], "global", "{written}");
    assert_eq!(written["dry_run"], false, "{written}");
    assert_eq!(
        written["written_to"],
        json!(dir.path().join("settings.json").display().to_string())
    );
    let mut changed: Vec<&str> = written["changed"]
        .as_array()
        .unwrap()
        .iter()
        .map(|k| k.as_str().unwrap())
        .collect();
    changed.sort();
    assert_eq!(changed, ["permissions.mode", "sandbox.allowed_domains"]);
    assert_eq!(written["rejected"], json!([]));
    assert_eq!(written["effective"]["permissions.mode"], "acceptEdits");
    assert_eq!(
        written["effective"]["sandbox.allowed_domains"],
        json!(["github.com"])
    );
    assert!(written["routing"].is_object(), "{written}");
    assert!(written.get("secrets_written_to").is_none(), "{written}");

    let tier = call(
        &sock,
        "config.get",
        json!({"scene": "code", "tier": "global"}),
    )
    .await;
    assert_eq!(
        tier["settings"]["permissions"]["mode"], "acceptEdits",
        "{tier}"
    );
    assert_eq!(
        tier["settings"]["sandbox"]["allowed_domains"],
        json!(["github.com"])
    );
    assert_eq!(tier["revision"], written["revision"], "{tier}");

    let merged = effective(&sock, json!({"scene": "code", "project_root": dir.path()})).await;
    assert_eq!(merged["permissions"]["mode"], "acceptEdits");
    assert_eq!(merged["sandbox"]["allowed_domains"], json!(["github.com"]));

    server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn a_scene_tier_write_reaches_that_scene_only() {
    let (server, sock, dir, handle) = start_server().await;
    let written = call(
        &sock,
        "config.set",
        json!({"tier": "scene", "scene": "harnix", "changes": {"language": "ja"}}),
    )
    .await;
    let scene_file = dir.path().join("scenes/harnix/settings.json");
    assert_eq!(
        written["written_to"],
        json!(scene_file.display().to_string())
    );
    assert_eq!(read(&scene_file)["language"], "ja");

    let harnix = effective(
        &sock,
        json!({"scene": "harnix", "project_root": dir.path()}),
    )
    .await;
    assert_eq!(harnix["language"], "ja", "{harnix}");
    let code = effective(&sock, json!({"scene": "code", "project_root": dir.path()})).await;
    assert!(code["language"].is_null(), "{code}");

    let tier = call(
        &sock,
        "config.get",
        json!({"scene": "harnix", "tier": "scene"}),
    )
    .await;
    assert_eq!(tier["settings"]["language"], "ja", "{tier}");

    server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn null_removes_the_key_from_the_tier_and_the_lower_value_shows_through() {
    let (server, sock, dir, handle) = start_server().await;
    let project = json!({"project_root": dir.path()});
    let set = |changes: Value| {
        let mut params = project.clone();
        params["tier"] = json!("project");
        params["changes"] = changes;
        params
    };
    call(
        &sock,
        "config.set",
        set(json!({"execution.max_api_calls_per_turn": 7})),
    )
    .await;
    call(
        &sock,
        "config.set",
        json!({"tier": "global", "changes": {"execution.max_api_calls_per_turn": 9}}),
    )
    .await;
    let mut scene = project.clone();
    scene["scene"] = json!("code");
    assert_eq!(
        effective(&sock, scene.clone()).await["execution"]["max_api_calls_per_turn"],
        7
    );

    let removed = call(
        &sock,
        "config.set",
        set(json!({"execution.max_api_calls_per_turn": null})),
    )
    .await;
    assert_eq!(
        removed["effective"]["execution.max_api_calls_per_turn"], 9,
        "{removed}"
    );
    let mut tier = scene.clone();
    tier["tier"] = json!("project");
    let tier = call(&sock, "config.get", tier).await;
    assert!(
        tier["settings"].get("execution").is_none(),
        "the emptied object is pruned: {tier}"
    );
    assert_eq!(
        effective(&sock, scene).await["execution"]["max_api_calls_per_turn"],
        9
    );

    server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn credentials_land_in_the_local_file_and_read_back_redacted() {
    let (server, sock, dir, handle) = start_server().await;
    let written = call(
        &sock,
        "config.set",
        json!({"tier": "global", "changes": {
            "providers.q": {"api_key": "token-123456", "base_url": "https://q.example"},
            "providers.p.api_key": "key-abcdef"
        }}),
    )
    .await;
    let (shared, local) = (
        dir.path().join("settings.json"),
        dir.path().join("settings.local.json"),
    );
    assert_eq!(
        written["secrets_written_to"],
        json!(local.display().to_string()),
        "{written}"
    );
    let committed = read(&shared);
    assert_eq!(
        committed["providers"],
        json!({"q": {"base_url": "https://q.example"}}),
        "{committed}"
    );
    let machine = read(&local);
    assert_eq!(machine["providers"]["q"]["api_key"], "token-123456");
    assert_eq!(machine["providers"]["p"]["api_key"], "key-abcdef");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&local).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "settings.local.json is {mode:o}");
    }
    assert_eq!(written["effective"]["providers.p.api_key"], "***cdef");

    let tier = call(
        &sock,
        "config.get",
        json!({"scene": "code", "tier": "global"}),
    )
    .await;
    assert_eq!(
        tier["local"]["providers"]["q"]["api_key"], "***3456",
        "{tier}"
    );
    assert_eq!(tier["local"]["providers"]["p"]["api_key"], "***cdef");

    server.shutdown_token().cancel();
    let _ = handle.await;
}

/// A credential inside a value — a password in a URL — is not a key the
/// writer moves to `settings.local.json`: it lands in `settings.json`, and
/// the answer says so without repeating it; read back, it is masked.
#[tokio::test]
async fn a_credential_inside_a_value_is_written_with_a_warning_and_read_back_masked() {
    let (server, sock, _dir, handle) = start_server().await;
    let written = call(
        &sock,
        "config.set",
        json!({"tier": "global", "changes": {
            "telemetry_url": "https://agent:hunter2-pass@collector.invalid/v1",
            "language": "en"
        }}),
    )
    .await;
    let warnings = written["warnings"].as_array().cloned().unwrap_or_default();
    assert_eq!(warnings.len(), 1, "{written}");
    assert!(
        warnings[0].as_str().unwrap().contains("telemetry_url"),
        "{written}"
    );
    assert!(!written.to_string().contains("hunter2-pass"), "{written}");

    let clean = call(
        &sock,
        "config.set",
        json!({"tier": "global", "changes": {"language": "ja"}}),
    )
    .await;
    assert!(clean.get("warnings").is_none(), "{clean}");

    server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn one_refused_key_refuses_the_write_and_each_says_why() {
    let (server, sock, dir, handle) = start_server().await;
    let files = [
        dir.path().join(".agix/settings.json"),
        dir.path().join(".agix/settings.local.json"),
    ];
    let before = bytes(&files);
    let changes = json!({
        "language": "fr",
        "sandbox.network_mode": "open",
        "hooks_config": {},
        "mcp_servers.x": {"type": "stdio", "command": "x"}
    });

    let refused = call_frame(
        &sock,
        "config.set",
        json!({"tier": "project", "changes": changes}),
    )
    .await;
    assert_eq!(refused["error"]["code"], codes::INVALID_PARAMS, "{refused}");
    let kinds = rejected_kinds(&refused["error"]["data"]["rejected"]);
    assert_eq!(
        kinds,
        [
            ("hooks_config".into(), "not_writable".into(), None),
            (
                "mcp_servers.x".into(),
                "use_method".into(),
                Some("mcp.addServer".into())
            ),
            ("sandbox.network_mode".into(), "invalid_value".into(), None),
        ],
        "{refused}"
    );
    assert_eq!(bytes(&files), before, "a refused write changed a file");

    let refused = call_frame(
        &sock,
        "config.set",
        json!({"tier": "project", "changes": {"language": "fr", "sandbox.network_mode": "open"}}),
    )
    .await;
    assert_eq!(refused["error"]["code"], codes::INVALID_PARAMS, "{refused}");
    assert_eq!(
        rejected_kinds(&refused["error"]["data"]["rejected"]),
        [("sandbox.network_mode".into(), "invalid_value".into(), None)],
        "{refused}"
    );
    assert!(
        refused["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("sandbox.network_mode")),
        "{refused}"
    );
    assert_eq!(bytes(&files), before, "a refused write changed a file");

    server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn a_dry_run_reports_the_keys_it_would_refuse_and_writes_nothing() {
    let (server, sock, dir, handle) = start_server().await;
    let files = [
        dir.path().join(".agix/settings.json"),
        dir.path().join(".agix/settings.local.json"),
    ];
    let before = bytes(&files);
    let result = call(
        &sock,
        "config.set",
        json!({"tier": "project", "dry_run": true, "changes": {
            "language": "fr",
            "sandbox.network_mode": "open",
            "hooks_config": {},
            "mcp_servers.x": {"type": "stdio", "command": "x"}
        }}),
    )
    .await;
    assert_eq!(result["dry_run"], true, "{result}");
    assert_eq!(result["changed"], json!([]), "{result}");
    assert_eq!(
        rejected_kinds(&result["rejected"]),
        [
            ("hooks_config".into(), "not_writable".into(), None),
            (
                "mcp_servers.x".into(),
                "use_method".into(),
                Some("mcp.addServer".into())
            ),
            ("sandbox.network_mode".into(), "invalid_value".into(), None),
        ],
        "{result}"
    );
    assert_eq!(bytes(&files), before, "a dry run changed a file");

    let accepted = call(
        &sock,
        "config.set",
        json!({"tier": "project", "dry_run": true, "changes": {"language": "fr"}}),
    )
    .await;
    assert_eq!(accepted["changed"], json!(["language"]), "{accepted}");
    assert_eq!(accepted["effective"]["language"], "fr");
    assert_eq!(bytes(&files), before, "a dry run changed a file");

    server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn an_untrusted_project_may_tighten_and_nothing_more() {
    let (server, sock, _dir, handle) = start_server().await;
    let elsewhere = tempfile::tempdir().unwrap();
    let project = elsewhere.path().join("repo");
    std::fs::create_dir_all(&project).unwrap();
    let set =
        |changes: Value| json!({"tier": "project", "project_root": project, "changes": changes});

    let tightened = call(
        &sock,
        "config.set",
        set(json!({"permissions.mode": "plan"})),
    )
    .await;
    assert_eq!(
        tightened["changed"],
        json!(["permissions.mode"]),
        "{tightened}"
    );
    assert_eq!(
        read(&project.join(".agix/settings.json"))["permissions"]["mode"],
        "plan"
    );

    let loosened = call_frame(
        &sock,
        "config.set",
        set(json!({"permissions.mode": "bypassPermissions"})),
    )
    .await;
    assert_eq!(
        loosened["error"]["code"],
        codes::PROJECT_UNTRUSTED,
        "{loosened}"
    );
    assert_eq!(
        rejected_kinds(&loosened["error"]["data"]["rejected"]),
        [("permissions.mode".into(), "requires_trust".into(), None)]
    );
    assert_eq!(
        read(&project.join(".agix/settings.json"))["permissions"]["mode"],
        "plan"
    );

    let allowed = call_frame(
        &sock,
        "config.set",
        set(json!({"permissions.allow": ["Bash"]})),
    )
    .await;
    assert_eq!(
        allowed["error"]["code"],
        codes::PROJECT_UNTRUSTED,
        "{allowed}"
    );

    let mixed = call_frame(
        &sock,
        "config.set",
        set(json!({"permissions.mode": "bypassPermissions", "language": 5})),
    )
    .await;
    assert_eq!(mixed["error"]["code"], codes::INVALID_PARAMS, "{mixed}");
    assert_eq!(
        rejected_kinds(&mixed["error"]["data"]["rejected"]).len(),
        2,
        "{mixed}"
    );

    server.shutdown_token().cancel();
    let _ = handle.await;
}

/// A repository whose `.agix` links to another project's: a write to its
/// tier would land in the other project — removing a trusted project's deny
/// rule on behalf of one that is not trusted — so it is refused and the other
/// project's file is left as it was.
#[cfg(unix)]
#[tokio::test]
async fn a_project_tier_linked_into_another_project_is_not_written() {
    let (server, sock, dir, handle) = start_server().await;
    let elsewhere = tempfile::tempdir().unwrap();
    let base = elsewhere.path().canonicalize().unwrap();
    let (linked, trusted) = (base.join("a"), base.join("b"));
    std::fs::create_dir_all(trusted.join(".agix")).unwrap();
    std::fs::create_dir_all(&linked).unwrap();
    let theirs = trusted.join(".agix/settings.json");
    std::fs::write(
        &theirs,
        json!({"permissions": {"deny": ["Bash(rm:*)"]}}).to_string(),
    )
    .unwrap();
    std::fs::write(
        dir.path().join("settings.local.json"),
        json!({"project_trust": {"trusted": [trusted]}}).to_string(),
    )
    .unwrap();
    std::os::unix::fs::symlink("../b/.agix", linked.join(".agix")).unwrap();
    let before = std::fs::read(&theirs).unwrap();

    let refused = call_frame(
        &sock,
        "config.set",
        json!({"tier": "project", "project_root": linked, "changes": {"permissions.deny": null}}),
    )
    .await;
    assert_eq!(refused["error"]["code"], codes::INVALID_PARAMS, "{refused}");
    assert_eq!(std::fs::read(&theirs).unwrap(), before);

    server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn a_global_only_key_is_refused_below_the_global_tier() {
    let (server, sock, _dir, handle) = start_server().await;
    let refused = call_frame(
        &sock,
        "config.set",
        json!({"tier": "project", "changes": {"network.use_system_proxy": false}}),
    )
    .await;
    assert_eq!(refused["error"]["code"], codes::INVALID_PARAMS, "{refused}");
    assert_eq!(
        rejected_kinds(&refused["error"]["data"]["rejected"]),
        [(
            "network.use_system_proxy".into(),
            "not_at_tier".into(),
            None
        )]
    );
    let in_the_project = call_frame(
        &sock,
        "config.set",
        json!({"tier": "project", "changes": {"worktree_dir": ".trees"}}),
    )
    .await;
    assert_eq!(
        rejected_kinds(&in_the_project["error"]["data"]["rejected"]),
        [("worktree_dir".into(), "not_at_tier".into(), None)],
        "{in_the_project}"
    );
    let in_the_scene = call_frame(
        &sock,
        "config.set",
        json!({"tier": "scene", "changes": {"worktree_dir": ".trees"}}),
    )
    .await;
    assert!(in_the_scene["error"].is_null(), "{in_the_scene}");
    let unknown = call_frame(
        &sock,
        "config.set",
        json!({"tier": "global", "changes": {"sandbox.nope": 1, "paths": {}, "model.auth_token": "t"}}),
    )
    .await;
    assert_eq!(
        rejected_kinds(&unknown["error"]["data"]["rejected"]),
        [
            ("model.auth_token".into(), "unknown_key".into(), None),
            ("paths".into(), "unknown_key".into(), None),
            ("sandbox.nope".into(), "unknown_key".into(), None),
        ],
        "{unknown}"
    );

    server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test(flavor = "multi_thread")]
async fn two_connections_writing_one_tier_keep_both_keys() {
    let (server, sock, _dir, handle) = start_server().await;
    let (a, b) = tokio::join!(
        call_frame(
            &sock,
            "config.set",
            json!({"tier": "global", "changes": {"language": "de"}})
        ),
        call_frame(
            &sock,
            "config.set",
            json!({"tier": "global", "changes": {"memory_enabled": false}})
        ),
    );
    assert!(a.get("result").is_some(), "{a}");
    assert!(b.get("result").is_some(), "{b}");
    let tier = call(
        &sock,
        "config.get",
        json!({"scene": "code", "tier": "global"}),
    )
    .await;
    assert_eq!(tier["settings"]["language"], "de", "{tier}");
    assert_eq!(tier["settings"]["memory_enabled"], false, "{tier}");

    server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn a_write_on_a_stale_revision_is_refused_with_the_current_one() {
    let (server, sock, dir, handle) = start_server().await;
    let read_tier = || {
        call(
            &sock,
            "config.get",
            json!({"scene": "code", "tier": "global"}),
        )
    };
    let first = read_tier().await["revision"].clone();
    assert!(first.as_str().unwrap().starts_with("r1:"), "{first}");

    let written = call(
        &sock,
        "config.set",
        json!({"tier": "global", "if_revision": first, "changes": {"sandbox.allowed_domains": ["a.com"]}}),
    )
    .await;
    let after = written["revision"].clone();
    assert_ne!(after, first);
    assert_eq!(read_tier().await["revision"], after);
    let file = std::fs::read(dir.path().join("settings.json")).unwrap();

    let stale = call_frame(
        &sock,
        "config.set",
        json!({"tier": "global", "if_revision": first, "changes": {"sandbox.allowed_domains": ["b.com"]}}),
    )
    .await;
    assert_eq!(stale["error"]["code"], codes::SETTINGS_CONFLICT, "{stale}");
    assert_eq!(stale["error"]["data"]["revision"], after, "{stale}");
    assert_eq!(
        std::fs::read(dir.path().join("settings.json")).unwrap(),
        file
    );

    server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_tier_held_by_another_process_is_refused_as_locked() {
    let (server, sock, dir, handle) = start_server().await;
    let tier_dir = dir.path().join(".agix");
    std::fs::create_dir_all(&tier_dir).unwrap();
    let held = std::fs::File::open(&tier_dir).unwrap();
    // SAFETY: `flock` only reads the descriptor, which `held` keeps open.
    let rc = unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(&held), libc::LOCK_EX) };
    assert_eq!(rc, 0);

    let refused = call_frame(
        &sock,
        "config.set",
        json!({"tier": "project", "changes": {"language": "it"}}),
    )
    .await;
    assert_eq!(
        refused["error"]["code"],
        codes::SETTINGS_LOCKED,
        "{refused}"
    );
    assert!(!tier_dir.join("settings.json").exists());
    drop(held);

    call(
        &sock,
        "config.set",
        json!({"tier": "project", "changes": {"language": "it"}}),
    )
    .await;
    assert_eq!(read(&tier_dir.join("settings.json"))["language"], "it");

    server.shutdown_token().cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn an_empty_or_malformed_write_is_invalid() {
    let (server, sock, _dir, handle) = start_server().await;
    for params in [
        json!({"tier": "global", "changes": {}}),
        json!({"changes": {"language": "x"}}),
        json!({"tier": "global", "changes": {"a..b": 1}}),
        json!({"tier": "global", "changes": {"sandbox": {}, "sandbox.read_anywhere": true}}),
        json!({"tier": "project", "project_root": null, "changes": {"language": "x"}}),
    ] {
        let refused = call_frame(&sock, "config.set", params.clone()).await;
        assert_eq!(
            refused["error"]["code"],
            codes::INVALID_PARAMS,
            "{params}: {refused}"
        );
    }
    let missing = call_frame(
        &sock,
        "config.set",
        json!({"tier": "project", "project_root": "/nonexistent/agix-x", "changes": {"language": "x"}}),
    )
    .await;
    assert_eq!(
        missing["error"]["code"],
        codes::PROJECT_NOT_FOUND,
        "{missing}"
    );
    let scene = call_frame(
        &sock,
        "config.set",
        json!({"tier": "scene", "scene": "no-such-scene", "changes": {"language": "x"}}),
    )
    .await;
    assert_eq!(scene["error"]["code"], codes::SCENE_NOT_FOUND, "{scene}");

    server.shutdown_token().cancel();
    let _ = handle.await;
}

/// `(key, kind, method)` of each refusal, sorted by key.
fn rejected_kinds(rejected: &Value) -> Vec<(String, String, Option<String>)> {
    let mut kinds: Vec<_> = rejected
        .as_array()
        .unwrap_or_else(|| panic!("no rejected list: {rejected}"))
        .iter()
        .map(|r| {
            assert!(r["reason"].as_str().is_some_and(|s| !s.is_empty()), "{r}");
            (
                r["key"].as_str().unwrap().to_string(),
                r["kind"].as_str().unwrap().to_string(),
                r["method"].as_str().map(String::from),
            )
        })
        .collect();
    kinds.sort();
    kinds
}

/// A write that changes nothing an MCP server is started with leaves the
/// pair's servers running: the wrapper counts each start of the toy server.
#[tokio::test]
#[ignore = "builds and spawns the toy MCP server"]
async fn a_write_that_does_not_touch_mcp_does_not_restart_its_servers() {
    let binary = crate::mcp_config::toy_mcp_server();
    let (server, sock, dir, handle) = start_server().await;
    let marker = dir.path().join("starts.log");
    let wrapper = dir.path().join("toy.sh");
    std::fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\necho start >> '{}'\nexec '{}'\n",
            marker.display(),
            binary.display()
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    call(
        &sock,
        "config.set",
        json!({"tier": "global", "changes": {"language": "en"}}),
    )
    .await;
    let added = call(
        &sock,
        "mcp.addServer",
        json!({"name": "toy", "config": {"type": "stdio", "command": wrapper}}),
    )
    .await;
    assert!(
        added["servers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["name"] == "toy" && s["state"] == "connected"),
        "{added}"
    );
    let starts = || {
        std::fs::read_to_string(&marker)
            .unwrap_or_default()
            .lines()
            .count()
    };
    let before = starts();

    call(
        &sock,
        "config.set",
        json!({"tier": "global", "changes": {"language": "de"}}),
    )
    .await;
    call(
        &sock,
        "config.set",
        json!({"tier": "project", "changes": {"memory_enabled": false}}),
    )
    .await;
    let status = call(&sock, "mcp.status", json!({})).await;
    assert!(
        status["servers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["name"] == "toy" && s["state"] == "connected"),
        "{status}"
    );
    assert_eq!(
        starts(),
        before,
        "a write that does not touch MCP restarted it"
    );

    server.shutdown_token().cancel();
    let _ = handle.await;
}

/// What the tier's `settings.local.json` holds beside a list `config.set`
/// writes to `settings.json` is not held against the write: an "always
/// allow" recorded there does not refuse deny rules written here.
#[tokio::test]
async fn an_allow_recorded_beside_the_list_does_not_refuse_a_deny_written_to_it() {
    let (server, sock, _dir, handle) = start_server().await;
    let elsewhere = tempfile::tempdir().unwrap();
    let project = elsewhere.path().join("repo");
    std::fs::create_dir_all(project.join(".agix")).unwrap();
    std::fs::write(
        project.join(".agix/settings.local.json"),
        r#"{"permissions": {"allow": ["Bash(ls)"]}}"#,
    )
    .unwrap();
    let written = call(
        &sock,
        "config.set",
        json!({"tier": "project", "project_root": project, "changes": {
            "permissions.deny": ["Bash(rm:*)"]
        }}),
    )
    .await;
    assert_eq!(written["changed"], json!(["permissions.deny"]), "{written}");
    assert_eq!(
        read(&project.join(".agix/settings.json"))["permissions"]["deny"],
        json!(["Bash(rm:*)"])
    );

    server.shutdown_token().cancel();
    let _ = handle.await;
}
