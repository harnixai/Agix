//! Another instance's state root on the same machine — `~/.agix`, seen from
//! an instance whose state root is elsewhere (an embedder's `data_root`, an
//! `AGIX_CONFIG_HOME`) — keeps its keys and its daemon to itself: a sandboxed
//! `Bash` command reads none of its settings, socket tokens or MCP OAuth
//! tokens and cannot connect to a socket in it, and the file tools refuse to
//! read or write its keys even under an allow rule.

use std::path::{Path, PathBuf};

use blackbox::{LiveConfig, LiveRun, Mode, ProviderStub, Reply};
use serde_json::json;

const SETTINGS_KEY: &str = "OTHER-INSTANCE-SETTINGS-KEY-1c7";
const SOCKET_TOKEN: &str = "OTHER-INSTANCE-SOCKET-TOKEN-8e2";
const OAUTH_TOKEN: &str = "OTHER-INSTANCE-OAUTH-TOKEN-3a9";
const REWRITTEN: &str = "REWRITTEN-BY-THE-MODEL";

/// Connects to the unix socket in `argv[1]`.
const CONNECT: &str = r#"
import socket, sys
s = socket.socket(socket.AF_UNIX)
try:
    s.connect(sys.argv[1])
    print("conn" + "ected")
except OSError as e:
    print("refused:", e)
"#;

fn result_of(
    events: &rpc_client::TurnEvents,
    matching: impl Fn(&str, &serde_json::Value) -> bool,
) -> (String, bool) {
    let at = events
        .tool_uses
        .iter()
        .position(|(name, input)| matching(name, input))
        .unwrap_or_else(|| panic!("the call was not made: {:?}", events.tool_uses));
    let (_, out, is_error) = events.tool_results[at].clone();
    (out, is_error)
}

fn write(path: &Path, contents: &str) -> anyhow::Result<()> {
    std::fs::create_dir_all(path.parent().expect("a parent"))?;
    std::fs::write(path, contents)?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn another_instances_state_root_keeps_its_keys_and_its_socket() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let home = tempfile::tempdir()?;
    let home_dir = home.path().canonicalize()?;
    let other = home_dir.join(".agix");
    let settings = other.join("settings.json");
    let token = other.join("daemon/auth/desktop.token");
    let oauth = other.join("mcp/oauth/server.json");
    write(&settings, &json!({ "api_key": SETTINGS_KEY }).to_string())?;
    write(&token, SOCKET_TOKEN)?;
    write(&oauth, &json!({ "access_token": OAUTH_TOKEN }).to_string())?;
    // A unix socket path is limited to about a hundred bytes; the scene
    // directory of a temporary home is short enough on both platforms.
    let socket: PathBuf = other.join("scenes/code/agixd.sock");
    std::fs::create_dir_all(socket.parent().expect("a parent"))?;
    let _listener = std::os::unix::net::UnixListener::bind(&socket)?;

    let home_env = home_dir.to_string_lossy().to_string();
    let mut run = LiveRun::builder("other-instance")
        .config(LiveConfig::for_endpoint(
            &stub.base_url(),
            blackbox::ModelEndpoint::model(&stub),
        ))
        .settings(json!({
            "permission_rules": [
                { "tool": "Bash", "action": "allow" },
                { "tool": "Read", "action": "allow" },
                { "tool": "Write", "action": "allow" }
            ]
        }))
        .mode(Mode::Spawned)
        .options(move |o| o.env("HOME", home_env.clone()))
        .start()
        .await?;
    let session = run.session().await?;
    let cat = format!(
        "cat '{}' '{}' '{}'; echo end",
        settings.display(),
        token.display(),
        oauth.display()
    );
    let connect = format!(
        "python3 -c '{}' '{}'",
        CONNECT.replace('\'', r"'\''"),
        socket.display()
    );
    stub.script([
        Reply::calls("b-cat", "Bash", json!({ "command": cat })),
        Reply::calls("b-connect", "Bash", json!({ "command": connect })),
        Reply::calls(
            "r-settings",
            "Read",
            json!({ "file_path": settings.to_string_lossy() }),
        ),
        Reply::calls(
            "w-settings",
            "Write",
            json!({ "file_path": settings.to_string_lossy(), "content": REWRITTEN }),
        ),
        Reply::text("done"),
    ]);
    let events = run.turn(&session, "look at the other instance").await?;

    let (shown, _) = result_of(&events, |_, i| i["command"] == cat);
    for secret in [SETTINGS_KEY, SOCKET_TOKEN, OAUTH_TOKEN] {
        assert!(!shown.contains(secret), "Bash read {secret}: {shown}");
    }
    let (connected, _) = result_of(&events, |_, i| i["command"] == connect);
    assert!(
        !connected.contains("connected"),
        "Bash reached the other instance's socket: {connected}"
    );
    let (read, _) = result_of(&events, |name, _| name == "Read");
    assert!(
        !read.contains(SETTINGS_KEY),
        "Read returned the key: {read}"
    );
    let (written, write_failed) = result_of(&events, |name, _| name == "Write");
    assert!(write_failed, "Write went through: {written}");
    assert!(
        std::fs::read_to_string(&settings)?.contains(SETTINGS_KEY),
        "the other instance's settings were rewritten"
    );
    run.finish_allowing_refusals().await;
    Ok(())
}
