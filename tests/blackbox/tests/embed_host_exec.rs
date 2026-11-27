//! `exec.run` / `exec.cancel` over the embed entry's own connection: offered
//! only there and only when the embedder opened it, and the command held to
//! the sandbox the request names — what it may write, read, reach and
//! inherit, how long it runs and how much of its output comes back.

use std::path::{Path, PathBuf};
use std::time::Duration;

use daemon::embed::{EmbedConfig, Embedded};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};

const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const EXEC_REFUSED: i64 = -32030;

struct Root {
    _dir: tempfile::TempDir,
    base: PathBuf,
    data: PathBuf,
    project: PathBuf,
}

fn root() -> anyhow::Result<Root> {
    let dir = tempfile::tempdir()?;
    let base = dir.path().canonicalize()?;
    let data = base.join("data");
    let project = base.join("project");
    std::fs::create_dir_all(&data)?;
    std::fs::create_dir_all(&project)?;
    Ok(Root {
        _dir: dir,
        base,
        data,
        project,
    })
}

fn config(root: &Root, host_exec: bool) -> EmbedConfig {
    let mut config = EmbedConfig::new(&root.data);
    config.credentials = Some(blackbox::stub_credentials());
    config.cwd = Some(root.project.clone());
    config.host_exec = host_exec;
    config
}

struct Wire<R, W> {
    reader: BufReader<R>,
    writer: W,
    next_id: u64,
}

impl<R: AsyncRead + Unpin, W: AsyncWrite + Unpin> Wire<R, W> {
    fn new(reader: R, writer: W) -> Self {
        Self {
            reader: BufReader::new(reader),
            writer,
            next_id: 1,
        }
    }

    async fn send(&mut self, method: &str, params: Value) -> anyhow::Result<u64> {
        let id = self.next_id;
        self.next_id += 1;
        let frame = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        self.writer
            .write_all(format!("{frame}\n").as_bytes())
            .await?;
        self.writer.flush().await?;
        Ok(id)
    }

    async fn reply(&mut self, id: u64) -> anyhow::Result<Value> {
        loop {
            let mut line = String::new();
            let read =
                tokio::time::timeout(Duration::from_secs(60), self.reader.read_line(&mut line))
                    .await??;
            anyhow::ensure!(read > 0, "the connection closed");
            let frame: Value = serde_json::from_str(line.trim())?;
            if frame.get("id") == Some(&json!(id)) {
                return Ok(frame);
            }
        }
    }

    async fn call(&mut self, method: &str, params: Value) -> anyhow::Result<Value> {
        let id = self.send(method, params).await?;
        self.reply(id).await
    }
}

type Duplex = Wire<
    tokio::io::ReadHalf<tokio::io::DuplexStream>,
    tokio::io::WriteHalf<tokio::io::DuplexStream>,
>;

fn duplex_wire(embedded: &Embedded) -> Duplex {
    let (ours, theirs) = tokio::io::duplex(1 << 20);
    embedded.serve_stream(theirs);
    let (r, w) = tokio::io::split(ours);
    Wire::new(r, w)
}

fn capabilities(pong: &Value) -> Vec<String> {
    pong["result"]["capabilities"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| c.as_str().map(str::to_string))
        .collect()
}

fn error_code(reply: &Value) -> Option<i64> {
    reply["error"]["code"].as_i64()
}

fn sh(script: &str, cwd: &Path) -> Value {
    json!({ "argv": ["sh", "-c", script], "cwd": cwd })
}

/// The reply to `params`, or `None` when this machine's sandbox cannot hold
/// a command at all — then the refusal, and the unconfined run the request
/// can ask for instead, are what is checked.
async fn run_confined(wire: &mut Duplex, params: Value) -> anyhow::Result<Option<Value>> {
    let reply = wire.call("exec.run", params.clone()).await?;
    if error_code(&reply) != Some(EXEC_REFUSED) {
        anyhow::ensure!(reply.get("result").is_some(), "exec.run failed: {reply}");
        return Ok(Some(reply["result"].clone()));
    }
    let mut unconfined = params;
    unconfined["allow_unconfined"] = json!(true);
    let ran = wire.call("exec.run", unconfined).await?;
    assert_eq!(ran["result"]["enforcement"], "none", "{ran}");
    Ok(None)
}

#[tokio::test(flavor = "multi_thread")]
async fn it_is_not_there_unless_the_embedder_opens_it() -> anyhow::Result<()> {
    let r = root()?;
    let embedded = Embedded::start(config(&r, false)).await?;
    let mut wire = duplex_wire(&embedded);
    let pong = wire.call("daemon.ping", json!({})).await?;
    assert!(!capabilities(&pong).contains(&"host_exec".to_string()));
    let reply = wire.call("exec.run", sh("true", &r.project)).await?;
    assert_eq!(error_code(&reply), Some(METHOD_NOT_FOUND), "{reply}");
    let reply = wire.call("exec.cancel", json!({ "run_id": "x" })).await?;
    assert_eq!(error_code(&reply), Some(METHOD_NOT_FOUND), "{reply}");
    embedded.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn only_the_connection_the_embedder_hands_over_has_it() -> anyhow::Result<()> {
    let r = root()?;
    let embedded = std::sync::Arc::new(Embedded::start(config(&r, true)).await?);
    let serving = {
        let embedded = embedded.clone();
        tokio::spawn(async move { embedded.serve_unix().await })
    };
    let mut own = duplex_wire(&embedded);
    let pong = own.call("daemon.status", json!({})).await?;
    assert!(
        capabilities(&pong).contains(&"host_exec".to_string()),
        "{pong}"
    );

    let socket = r.data.join("scenes/code/daemon.sock");
    let stream = loop {
        match tokio::net::UnixStream::connect(&socket).await {
            Ok(stream) => break stream,
            Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
        }
    };
    let (reader, writer) = tokio::io::split(stream);
    let mut other = Wire::new(reader, writer);
    let pong = other.call("daemon.ping", json!({})).await?;
    assert!(
        !capabilities(&pong).contains(&"host_exec".to_string()),
        "{pong}"
    );
    let reply = other.call("exec.run", sh("true", &r.project)).await?;
    assert_eq!(error_code(&reply), Some(METHOD_NOT_FOUND), "{reply}");
    for params in [json!({}), json!({ "nope": 1 })] {
        let reply = other.call("exec.run", params).await?;
        assert_eq!(error_code(&reply), Some(METHOD_NOT_FOUND), "{reply}");
    }

    embedded.shutdown().await;
    serving.await??;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_read_command_writes_only_its_own_tmpdir() -> anyhow::Result<()> {
    let r = root()?;
    let embedded = Embedded::start(config(&r, true)).await?;
    let mut wire = duplex_wire(&embedded);
    let script = r#"echo kept > "$TMPDIR/t" && cat "$TMPDIR/t"; echo x > ./out"#;
    let Some(result) = run_confined(&mut wire, sh(script, &r.project)).await? else {
        embedded.shutdown().await;
        return Ok(());
    };
    for field in [
        "code",
        "signal",
        "stdout",
        "stderr",
        "truncated",
        "timed_out",
        "cancelled",
        "ms",
        "enforcement",
        "unmet",
    ] {
        assert!(result.get(field).is_some(), "no `{field}` in {result}");
    }
    #[cfg(target_os = "macos")]
    assert_eq!(result["enforcement"], "full", "{result}");
    assert_eq!(
        result["stdout"].as_str().map(str::trim),
        Some("kept"),
        "{result}"
    );
    assert_ne!(result["code"], 0, "{result}");
    assert!(!r.project.join("out").exists(), "the command wrote its cwd");

    let tmp_root = r.data.join(base::paths::STATE_ROOT_SESSION_TMP);
    let left: Vec<_> = std::fs::read_dir(&tmp_root)
        .map(|d| d.flatten().map(|e| e.file_name()).collect())
        .unwrap_or_default();
    assert!(
        !left
            .iter()
            .any(|n| n.to_string_lossy().starts_with("exec-")),
        "the call's TMPDIR outlived it: {left:?}"
    );
    embedded.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_write_command_writes_what_it_names_and_nothing_else() -> anyhow::Result<()> {
    let r = root()?;
    let out = r.base.join("out");
    std::fs::create_dir_all(&out)?;
    let embedded = Embedded::start(config(&r, true)).await?;
    let mut wire = duplex_wire(&embedded);
    let mut params = sh(
        &format!(
            "echo a > '{}/a'; echo b > '{}/b'",
            out.display(),
            r.project.display()
        ),
        &r.project,
    );
    params["profile"] = json!("write");
    params["writable"] = json!([out]);
    if run_confined(&mut wire, params).await?.is_some() {
        assert!(out.join("a").exists(), "the named root was not writable");
        assert!(
            !r.project.join("b").exists(),
            "the command wrote outside it"
        );
    }

    let mut read_with_writable = sh("true", &r.project);
    read_with_writable["writable"] = json!([out]);
    let reply = wire.call("exec.run", read_with_writable).await?;
    assert_eq!(error_code(&reply), Some(INVALID_PARAMS), "{reply}");
    embedded.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn no_settings_file_lets_a_host_command_read_what_is_denied() -> anyhow::Result<()> {
    let r = root()?;
    let secret = r.base.join("secret.txt");
    let other = r.base.join("other.txt");
    std::fs::write(&secret, "SECRET")?;
    std::fs::write(&other, "OTHER")?;
    std::fs::write(
        r.data.join("settings.json"),
        serde_json::to_vec(
            &json!({ "sandbox": { "deny_read": [other], "allow_read": [secret] } }),
        )?,
    )?;
    let embedded = Embedded::start(config(&r, true)).await?;
    let mut wire = duplex_wire(&embedded);
    let mut params = sh(
        &format!("cat '{}'; cat '{}'", secret.display(), other.display()),
        &r.project,
    );
    params["deny_read"] = json!([secret]);
    if let Some(result) = run_confined(&mut wire, params).await? {
        #[cfg(target_os = "macos")]
        assert_eq!(result["enforcement"], "full", "{result}");
        let stdout = result["stdout"].as_str().unwrap_or_default();
        assert!(
            !stdout.contains("SECRET"),
            "deny_read let it through: {result}"
        );
        assert!(
            !stdout.contains("OTHER"),
            "sandbox.deny_read let it through: {result}"
        );
    }
    embedded.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn the_environment_is_the_minimal_one_and_what_the_request_adds() -> anyhow::Result<()> {
    let r = root()?;
    let embedded = Embedded::start(config(&r, true)).await?;
    let mut wire = duplex_wire(&embedded);
    let mut params = json!({ "argv": ["env"], "cwd": r.project });
    params["env"] = json!({ "HOST_EXEC_MARK": "here" });
    let reply = wire
        .call(
            "exec.run",
            json!({ "argv": ["env"], "cwd": r.project, "env": {"HOST_EXEC_MARK": "here"},
                    "allow_unconfined": true }),
        )
        .await?;
    let stdout = reply["result"]["stdout"].as_str().unwrap_or_default();
    assert!(stdout.contains("HOST_EXEC_MARK=here"), "{reply}");
    assert!(stdout.contains("TMPDIR="), "{reply}");
    if std::env::var_os("CARGO_MANIFEST_DIR").is_some() {
        assert!(
            !stdout.contains("CARGO_MANIFEST_DIR="),
            "inherited: {reply}"
        );
    }

    params["env"] = json!({ "DYLD_INSERT_LIBRARIES": "/x.dylib" });
    let reply = wire.call("exec.run", params.clone()).await?;
    assert_eq!(error_code(&reply), Some(INVALID_PARAMS), "{reply}");
    params["env"] = json!({});
    params["deny_reads"] = json!([]);
    let reply = wire.call("exec.run", params).await?;
    assert_eq!(error_code(&reply), Some(INVALID_PARAMS), "{reply}");
    embedded.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn the_time_limit_stops_it_and_the_output_limit_cuts_it() -> anyhow::Result<()> {
    let r = root()?;
    let embedded = Embedded::start(config(&r, true)).await?;
    let mut wire = duplex_wire(&embedded);

    let mut slow = sh("sleep 30", &r.project);
    slow["timeout_ms"] = json!(300);
    slow["allow_unconfined"] = json!(true);
    let reply = wire.call("exec.run", slow).await?;
    assert_eq!(reply["result"]["timed_out"], true, "{reply}");
    assert_ne!(reply["result"]["code"], 0, "{reply}");

    let mut loud = sh(
        "head -c 5000 /dev/zero | tr '\\0' a; echo err >&2",
        &r.project,
    );
    loud["max_output_bytes"] = json!(100);
    loud["allow_unconfined"] = json!(true);
    let reply = wire.call("exec.run", loud).await?;
    assert_eq!(reply["result"]["truncated"], true, "{reply}");
    assert_eq!(reply["result"]["stdout"].as_str().map(str::len), Some(100));
    assert_eq!(
        reply["result"]["stderr"].as_str().map(str::trim),
        Some("err")
    );

    let mut over = sh("true", &r.project);
    over["timeout_ms"] = json!(1_800_001);
    let reply = wire.call("exec.run", over).await?;
    assert_eq!(error_code(&reply), Some(INVALID_PARAMS), "{reply}");
    embedded.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn exec_cancel_stops_the_call_it_names() -> anyhow::Result<()> {
    let r = root()?;
    let embedded = Embedded::start(config(&r, true)).await?;
    let mut wire = duplex_wire(&embedded);
    let mut slow = sh("sleep 30", &r.project);
    slow["run_id"] = json!("slow-1");
    slow["allow_unconfined"] = json!(true);
    let running = wire.send("exec.run", slow).await?;
    loop {
        let reply = wire
            .call("exec.cancel", json!({ "run_id": "slow-1" }))
            .await?;
        if reply["result"]["cancelled"] == true {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let reply = wire.reply(running).await?;
    assert_eq!(reply["result"]["cancelled"], true, "{reply}");
    assert_eq!(reply["result"]["timed_out"], false, "{reply}");
    embedded.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn the_network_is_cut_unless_the_request_opens_it() -> anyhow::Result<()> {
    let r = root()?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            drop(stream);
        }
    });
    let embedded = Embedded::start(config(&r, true)).await?;
    let mut wire = duplex_wire(&embedded);
    let connect = |network: bool| {
        json!({ "argv": ["bash", "-c", format!("exec 3<>/dev/tcp/127.0.0.1/{port}")],
                "cwd": r.project, "network": network })
    };
    let Some(cut) = run_confined(&mut wire, connect(false)).await? else {
        embedded.shutdown().await;
        return Ok(());
    };
    if cut["enforcement"] == "full" {
        assert_ne!(cut["code"], 0, "connected with the network cut: {cut}");
    }
    let open = wire.call("exec.run", connect(true)).await?;
    assert_eq!(open["result"]["code"], 0, "{open}");
    embedded.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_managed_deny_rule_refuses_the_command_it_names() -> anyhow::Result<()> {
    let r = root()?;
    let managed = r.base.join("managed-settings.json");
    std::fs::write(&managed, r#"{"permissions": {"deny": ["Bash(touch:*)"]}}"#)?;
    let mut config = config(&r, true);
    config.managed_settings = Some(managed);
    let embedded = Embedded::start(config).await?;
    let mut wire = duplex_wire(&embedded);
    let target = r.base.join("touched");
    let reply = wire
        .call(
            "exec.run",
            json!({ "argv": ["/usr/bin/touch", target], "cwd": r.project,
                    "allow_unconfined": true }),
        )
        .await?;
    assert_eq!(error_code(&reply), Some(EXEC_REFUSED), "{reply}");
    assert!(!target.exists());
    let reply = wire
        .call(
            "exec.run",
            json!({ "argv": ["true"], "cwd": r.project, "allow_unconfined": true }),
        )
        .await?;
    assert_eq!(reply["result"]["code"], 0, "{reply}");
    embedded.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn the_request_path_does_not_choose_the_sandbox() -> anyhow::Result<()> {
    let r = root()?;
    let fake = r.base.join("fakebin");
    std::fs::create_dir_all(&fake)?;
    let marker = r.base.join("escaped");
    for wrapper in ["sandbox-exec", "sh", "bwrap"] {
        let script = fake.join(wrapper);
        std::fs::write(
            &script,
            format!("#!/bin/sh\ntouch '{}'\n", marker.display()),
        )?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))?;
        }
    }
    let embedded = Embedded::start(config(&r, true)).await?;
    let mut wire = duplex_wire(&embedded);
    let mut params = json!({ "argv": ["true"], "cwd": r.project });
    params["env"] = json!({ "PATH": format!("{}:/usr/bin:/bin", fake.display()) });
    let ran = run_confined(&mut wire, params).await?;
    if ran.is_some() {
        assert!(
            !marker.exists(),
            "the request's PATH chose the sandbox wrapper"
        );
    }

    let reply = wire
        .call(
            "exec.run",
            json!({ "argv": ["true"], "cwd": r.project,
                    "env": {"BASH_FUNC_bwrap%%": "() { true; }"} }),
        )
        .await?;
    assert_eq!(error_code(&reply), Some(INVALID_PARAMS), "{reply}");
    embedded.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_writable_root_cannot_reach_a_projects_configuration() -> anyhow::Result<()> {
    let r = root()?;
    let pkg = r.project.join("pkg");
    let other = r.base.join("other");
    std::fs::create_dir_all(&pkg)?;
    std::fs::create_dir_all(other.join(".git"))?;
    let embedded = Embedded::start(config(&r, true)).await?;
    let mut wire = duplex_wire(&embedded);
    for root in [&r.project, &other] {
        let reply = wire
            .call(
                "exec.run",
                json!({ "argv": ["true"], "cwd": pkg, "profile": "write", "writable": [root] }),
            )
            .await?;
        assert_eq!(
            error_code(&reply),
            Some(INVALID_PARAMS),
            "{root:?}: {reply}"
        );
    }
    let mut inside = sh("echo in > ./made", &pkg);
    inside["profile"] = json!("write");
    inside["writable"] = json!([pkg]);
    if run_confined(&mut wire, inside).await?.is_some() {
        assert!(pkg.join("made").exists());
    }
    embedded.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_managed_read_deny_rule_keeps_the_file_unread() -> anyhow::Result<()> {
    let r = root()?;
    let secret = r.base.join("corp-secret.txt");
    std::fs::write(&secret, "CORP")?;
    let managed = r.base.join("managed-settings.json");
    std::fs::write(
        &managed,
        serde_json::to_vec(
            &json!({ "permissions": { "deny": [format!("Read({})", secret.display())] } }),
        )?,
    )?;
    let mut config = config(&r, true);
    config.managed_settings = Some(managed);
    let embedded = Embedded::start(config).await?;
    let mut wire = duplex_wire(&embedded);
    let params = sh(&format!("cat '{}'", secret.display()), &r.project);
    if let Some(result) = run_confined(&mut wire, params).await? {
        assert!(
            !result["stdout"]
                .as_str()
                .unwrap_or_default()
                .contains("CORP"),
            "{result}"
        );
    }
    embedded.shutdown().await;
    Ok(())
}
