//! The embed entry (`daemon::embed`) over its real surface: one protocol path
//! for a stream and a unix socket, data roots that do not leak into each
//! other, a named instance's files, shutdown, and the scene check.

use std::path::{Path, PathBuf};
use std::time::Duration;

use blackbox::{ProviderStub, Reply};
use daemon::embed::{EmbedConfig, EmbedError, Embedded};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};

const TITLE_NEEDLE: &str = "标题生成器";

/// A process runs at most one named instance: the cases that start one take
/// turns.
static NAMED: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn write_provider_settings(data_root: &Path, stub: &ProviderStub) -> anyhow::Result<()> {
    std::fs::create_dir_all(data_root)?;
    let settings = json!({
        "providers": { "stub": {
            "api_type": "anthropic",
            "base_url": stub.base_url().as_str(),
            "api_key": "stub-key",
            "default_model": "claude-sonnet-4-6",
            "models": ["claude-sonnet-4-6"]
        }},
        "default_provider": "stub"
    });
    std::fs::write(
        data_root.join("settings.json"),
        serde_json::to_vec_pretty(&settings)?,
    )?;
    Ok(())
}

struct Root {
    _dir: tempfile::TempDir,
    data: PathBuf,
    project: PathBuf,
}

fn root(stub: &ProviderStub) -> anyhow::Result<Root> {
    let dir = tempfile::tempdir()?;
    let dir_path = dir.path().canonicalize()?;
    let data = dir_path.join("data");
    let project = dir_path.join("project");
    std::fs::create_dir_all(&project)?;
    write_provider_settings(&data, stub)?;
    Ok(Root {
        _dir: dir,
        data,
        project,
    })
}

fn config(root: &Root) -> EmbedConfig {
    let mut config = EmbedConfig::new(&root.data);
    config.cwd = Some(root.project.clone());
    config
}

async fn start(root: &Root) -> anyhow::Result<Embedded> {
    Ok(Embedded::start(config(root)).await?)
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

    async fn frame(&mut self) -> anyhow::Result<Value> {
        let mut line = String::new();
        let read = tokio::time::timeout(Duration::from_secs(60), self.reader.read_line(&mut line))
            .await??;
        anyhow::ensure!(read > 0, "the connection closed");
        Ok(serde_json::from_str(line.trim())?)
    }

    async fn call_collecting(
        &mut self,
        method: &str,
        params: Value,
    ) -> anyhow::Result<(Vec<Value>, Value)> {
        let id = self.send(method, params).await?;
        let mut before = Vec::new();
        loop {
            let frame = self.frame().await?;
            if frame.get("id") == Some(&json!(id)) {
                return Ok((before, frame));
            }
            before.push(frame);
        }
    }

    async fn call(&mut self, method: &str, params: Value) -> anyhow::Result<Value> {
        Ok(self.call_collecting(method, params).await?.1)
    }
}

fn duplex_wire(
    embedded: &Embedded,
) -> Wire<tokio::io::ReadHalf<tokio::io::DuplexStream>, tokio::io::WriteHalf<tokio::io::DuplexStream>>
{
    let (ours, theirs) = tokio::io::duplex(1 << 20);
    embedded.serve_stream(theirs);
    let (r, w) = tokio::io::split(ours);
    Wire::new(r, w)
}

async fn unix_wire(
    socket: &Path,
) -> anyhow::Result<
    Wire<tokio::io::ReadHalf<tokio::net::UnixStream>, tokio::io::WriteHalf<tokio::net::UnixStream>>,
> {
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        match tokio::net::UnixStream::connect(socket).await {
            Ok(stream) => {
                let (r, w) = tokio::io::split(stream);
                return Ok(Wire::new(r, w));
            }
            Err(e) if std::time::Instant::now() > deadline => return Err(e.into()),
            Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
        }
    }
}

fn socket_of(root: &Root) -> PathBuf {
    root.data.join("scenes/code/daemon.sock")
}

fn script_plain_turn(stub: &ProviderStub) {
    stub.script_when(TITLE_NEEDLE, (0..4).map(|_| Reply::text("a title")));
    stub.script([Reply::text("EMBED-ANSWER")]);
}

async fn plain_turn<R, W>(wire: &mut Wire<R, W>) -> anyhow::Result<(Vec<Value>, Vec<Value>)>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut frames = Vec::new();
    let (_, ping) = wire.call_collecting("daemon.ping", json!({})).await?;
    frames.push(ping);
    let (events, created) = wire.call_collecting("session.create", json!({})).await?;
    frames.extend(events);
    let session = created["result"]["session_id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("no session id: {created}"))?
        .to_string();
    frames.push(created);
    let (events, done) = wire
        .call_collecting(
            "session.run_turn",
            json!({ "session_id": session, "message": "say something", "turn_id": "t1" }),
        )
        .await?;
    let turn_events = events.clone();
    frames.extend(events);
    frames.push(done);
    Ok((frames, turn_events))
}

fn normalize(value: &Value, ids: &[String], roots: &[String]) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| {
                    let run_specific = k == "id"
                        || k == "seq"
                        || k == "usage"
                        || k.contains("time")
                        || k.ends_with("_at")
                        || k.ends_with("_ms")
                        || k.ends_with("_secs")
                        || k.contains("duration")
                        || k == "pid"
                        || k == "ts";
                    if run_specific {
                        (k.clone(), json!("<run-specific>"))
                    } else {
                        (k.clone(), normalize(v, ids, roots))
                    }
                })
                .collect(),
        ),
        Value::Array(items) => {
            Value::Array(items.iter().map(|v| normalize(v, ids, roots)).collect())
        }
        Value::String(s) => {
            let mut s = s.clone();
            for id in ids {
                s = s.replace(id.as_str(), "<id>");
            }
            for root in roots {
                s = s.replace(root.as_str(), "<root>");
            }
            Value::String(s)
        }
        other => other.clone(),
    }
}

fn session_ids(frames: &[Value]) -> Vec<String> {
    let mut ids = Vec::new();
    for f in frames {
        if let Some(id) = f["result"]["session_id"].as_str() {
            ids.push(id.to_string());
        }
    }
    ids
}

fn is_event(frame: &Value) -> bool {
    frame["method"] == "session.event"
}

/// A duplex stream and a unix socket produce the same normalized frame sequence, and on a stream the turn's response follows its events.
#[tokio::test(flavor = "multi_thread")]
async fn a_stream_and_a_unix_socket_carry_the_same_protocol() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;

    let a = root(&stub)?;
    let embedded = start(&a).await?;
    script_plain_turn(&stub);
    let mut wire = duplex_wire(&embedded);
    let (duplex_frames, turn_events) = plain_turn(&mut wire).await?;
    assert!(
        turn_events.iter().any(is_event),
        "the turn produced no session.event frames: {turn_events:?}"
    );
    let streamed: String = duplex_frames
        .iter()
        .filter(|f| f["params"]["event"]["kind"] == "text_delta")
        .filter_map(|f| f["params"]["event"]["text"].as_str())
        .collect();
    assert_eq!(streamed, "EMBED-ANSWER");
    let last_event = duplex_frames.iter().rposition(is_event).unwrap();
    let response = duplex_frames.len() - 1;
    assert!(
        last_event < response,
        "run_turn's response came before the turn's events: {duplex_frames:#?}"
    );
    assert_eq!(duplex_frames[response]["id"], json!(wire.next_id - 1));
    embedded.shutdown().await;

    let b = root(&stub)?;
    let embedded = std::sync::Arc::new(start(&b).await?);
    let serving = {
        let embedded = embedded.clone();
        tokio::spawn(async move { embedded.serve_unix().await })
    };
    script_plain_turn(&stub);
    let mut wire = unix_wire(&socket_of(&b)).await?;
    let (unix_frames, _) = plain_turn(&mut wire).await?;
    embedded.shutdown().await;
    serving.await??;

    let roots = [
        a.data.display().to_string(),
        a.project.display().to_string(),
        b.data.display().to_string(),
        b.project.display().to_string(),
    ];
    let mut duplex_ids = session_ids(&duplex_frames);
    duplex_ids.push("t1".into());
    let mut unix_ids = session_ids(&unix_frames);
    unix_ids.push("t1".into());
    let left: Vec<Value> = duplex_frames
        .iter()
        .map(|f| normalize(f, &duplex_ids, &roots))
        .collect();
    let right: Vec<Value> = unix_frames
        .iter()
        .map(|f| normalize(f, &unix_ids, &roots))
        .collect();
    assert_eq!(
        left, right,
        "the two transports diverged:\nduplex: {left:#?}\nunix: {right:#?}"
    );
    Ok(())
}

/// Two engines on different data roots in one process keep their sessions apart.
#[tokio::test(flavor = "multi_thread")]
async fn two_data_roots_in_one_process_do_not_see_each_other() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let a = root(&stub)?;
    let b = root(&stub)?;
    let (ea, eb) = (start(&a).await?, start(&b).await?);
    let (mut wa, mut wb) = (duplex_wire(&ea), duplex_wire(&eb));

    let ca = wa.call("session.create", json!({})).await?;
    let cb = wb.call("session.create", json!({})).await?;
    let id_a = ca["result"]["session_id"].as_str().unwrap().to_string();
    let id_b = cb["result"]["session_id"].as_str().unwrap().to_string();

    let list_a = wa.call("session.list", Value::Null).await?.to_string();
    let list_b = wb.call("session.list", Value::Null).await?.to_string();
    assert!(
        list_a.contains(&id_a) && !list_a.contains(&id_b),
        "{list_a}"
    );
    assert!(
        list_b.contains(&id_b) && !list_b.contains(&id_a),
        "{list_b}"
    );

    let foreign = wa
        .call("session.get", json!({ "session_id": id_b }))
        .await?;
    assert_eq!(foreign["error"]["code"], json!(-32000), "{foreign}");

    ea.shutdown().await;
    eb.shutdown().await;
    Ok(())
}

fn dir_entries(dir: &Path) -> Vec<String> {
    match std::fs::read_dir(dir) {
        Ok(entries) => entries
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect(),
        Err(_) => Vec::new(),
    }
}

async fn wait_for(path: &Path) -> anyhow::Result<()> {
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while !path.exists() {
        anyhow::ensure!(
            std::time::Instant::now() < deadline,
            "{} never appeared",
            path.display()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Ok(())
}

/// Without an instance name nothing is written; a named one holds its lock and entry until shutdown and refuses a second claim.
#[tokio::test(flavor = "multi_thread")]
async fn only_a_named_instance_writes_discovery_files() -> anyhow::Result<()> {
    let _named = NAMED.lock().await;
    let stub = ProviderStub::start().await?;

    let anonymous = root(&stub)?;
    let embedded = std::sync::Arc::new(start(&anonymous).await?);
    let serving = {
        let embedded = embedded.clone();
        tokio::spawn(async move { embedded.serve_unix().await })
    };
    let mut wire = unix_wire(&socket_of(&anonymous)).await?;
    let pong = wire.call("daemon.ping", json!({})).await?;
    assert!(pong["error"].is_null(), "{pong}");
    embedded.shutdown().await;
    serving.await??;
    assert!(dir_entries(&anonymous.data.join("daemon/instances.d")).is_empty());
    assert!(dir_entries(&anonymous.data.join("daemon/groups")).is_empty());
    assert!(!anonymous.data.join("scenes/code/daemon.lock").exists());

    let named = root(&stub)?;
    let mut named_config = config(&named);
    named_config.instance = Some("embed-test".into());
    let embedded = std::sync::Arc::new(Embedded::start(named_config.clone()).await?);
    let serving = {
        let embedded = embedded.clone();
        tokio::spawn(async move { embedded.serve_unix().await })
    };
    let announced = named.data.join("daemon/instances.d/embed-test.json");
    let lock = named.data.join("scenes/code/daemon.lock");
    wait_for(&announced).await?;
    assert!(lock.exists());
    match Embedded::start(named_config).await {
        Err(EmbedError::AlreadyRunning { .. }) => {}
        Err(other) => panic!("expected AlreadyRunning, got {other:?}"),
        Ok(_) => panic!("a second engine took a held instance"),
    }
    embedded.shutdown().await;
    serving.await??;
    assert!(
        !announced.exists(),
        "the instances.d entry outlived shutdown"
    );
    assert!(!lock.exists(), "the lock outlived shutdown");
    Ok(())
}

/// `daemon.shutdown` resolves `closed()`, ends `serve_unix`, and `shutdown()` is idempotent and safe to call concurrently.
#[tokio::test(flavor = "multi_thread")]
async fn a_daemon_shutdown_rpc_resolves_closed_and_ends_serve_unix() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let r = root(&stub)?;
    let embedded = std::sync::Arc::new(start(&r).await?);
    let serving = {
        let embedded = embedded.clone();
        tokio::spawn(async move { embedded.serve_unix().await })
    };
    wait_for(&socket_of(&r)).await?;

    let mut wire = duplex_wire(&embedded);
    let _ = wire.call("daemon.shutdown", json!({})).await;

    tokio::time::timeout(Duration::from_secs(10), embedded.closed()).await?;
    tokio::time::timeout(Duration::from_secs(10), serving).await???;

    let (first, second) = tokio::join!(
        {
            let embedded = embedded.clone();
            tokio::spawn(async move { embedded.shutdown().await })
        },
        {
            let embedded = embedded.clone();
            tokio::spawn(async move { embedded.shutdown().await })
        }
    );
    first?;
    second?;
    tokio::time::timeout(Duration::from_secs(15), embedded.shutdown()).await?;
    Ok(())
}

/// An unknown scene name fails start and lists the known scenes.
#[tokio::test(flavor = "multi_thread")]
async fn an_unknown_scene_is_refused_with_the_known_ones() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let r = root(&stub)?;
    let mut c = config(&r);
    c.scene = "no-such-scene".into();
    match Embedded::start(c).await {
        Err(EmbedError::UnknownScene { scene, known }) => {
            assert_eq!(scene, "no-such-scene");
            assert!(known.iter().any(|k| k == "code"), "{known:?}");
        }
        Err(other) => panic!("expected UnknownScene, got {other:?}"),
        Ok(_) => panic!("an unknown scene started"),
    }
    Ok(())
}

/// An engine without an instance name, on the data root of one that is
/// serving, does not take the socket from under it.
#[tokio::test(flavor = "multi_thread")]
async fn an_anonymous_engine_does_not_take_over_a_served_socket() -> anyhow::Result<()> {
    let _named = NAMED.lock().await;
    let stub = ProviderStub::start().await?;
    let r = root(&stub)?;
    let mut named = config(&r);
    named.instance = Some("served".into());
    let first = std::sync::Arc::new(Embedded::start(named).await?);
    let serving = {
        let first = first.clone();
        tokio::spawn(async move { first.serve_unix().await })
    };
    wait_for(&r.data.join("daemon/instances.d/served.json")).await?;

    let second = std::sync::Arc::new(start(&r).await?);
    let taking = {
        let second = second.clone();
        tokio::spawn(async move { second.serve_unix().await })
    };
    let outcome = tokio::time::timeout(Duration::from_secs(3), taking).await;
    second.shutdown().await;
    first.shutdown().await;
    serving.await??;
    match outcome {
        Ok(Ok(Err(e))) if e.kind() == std::io::ErrorKind::AddrInUse => Ok(()),
        Ok(Ok(Err(e))) => panic!("serve_unix failed otherwise: {e}"),
        Ok(Ok(Ok(()))) => panic!("serve_unix ended without refusing"),
        Ok(Err(e)) => panic!("serve_unix panicked: {e}"),
        Err(_) => panic!(
            "a second engine unlinked the socket a running one serves and listens there itself"
        ),
    }
}

/// `serve_unix` called once shutdown has run announces nothing it leaves behind.
#[tokio::test(flavor = "multi_thread")]
async fn serve_unix_after_shutdown_leaves_no_discovery_entry() -> anyhow::Result<()> {
    let _named = NAMED.lock().await;
    let stub = ProviderStub::start().await?;
    let r = root(&stub)?;
    let mut named = config(&r);
    named.instance = Some("late".into());
    let embedded = Embedded::start(named).await?;
    embedded.shutdown().await;
    let _ = tokio::time::timeout(Duration::from_secs(5), embedded.serve_unix()).await;
    let entry = r.data.join("daemon/instances.d/late.json");
    assert!(
        !entry.exists(),
        "a stopped engine announced itself: {}",
        std::fs::read_to_string(&entry).unwrap_or_default()
    );
    Ok(())
}

/// A named instance whose start fails gives back what it claimed.
#[tokio::test(flavor = "multi_thread")]
async fn a_named_instance_that_fails_to_start_leaves_no_files() -> anyhow::Result<()> {
    let _named = NAMED.lock().await;
    let stub = ProviderStub::start().await?;
    let r = root(&stub)?;
    let mut broken = config(&r);
    broken.instance = Some("broken".into());
    broken.scenes = vec!["no-such-scene".into()];
    match Embedded::start(broken).await {
        Err(EmbedError::Startup(_)) => {}
        Err(other) => panic!("expected Startup, got {other:?}"),
        Ok(_) => panic!("an unknown extra scene started"),
    }
    assert!(
        !r.data.join("scenes/code/daemon.lock").exists(),
        "the lock outlived the failed start"
    );
    let groups = r.data.join("daemon/groups/broken.json");
    assert!(
        !groups.exists(),
        "the failed start left its process-group file: {}",
        std::fs::read_to_string(&groups).unwrap_or_default()
    );
    let again = Embedded::start({
        let mut c = config(&r);
        c.instance = Some("broken".into());
        c
    })
    .await?;
    again.shutdown().await;
    Ok(())
}

/// One process, one named instance: a second is refused until the first stops.
#[tokio::test(flavor = "multi_thread")]
async fn a_process_runs_at_most_one_named_instance() -> anyhow::Result<()> {
    let _named = NAMED.lock().await;
    let stub = ProviderStub::start().await?;
    let (a, b) = (root(&stub)?, root(&stub)?);
    let named = |r: &Root, name: &str| {
        let mut c = config(r);
        c.instance = Some(name.into());
        c
    };
    let first = Embedded::start(named(&a, "one")).await?;
    match Embedded::start(named(&b, "two")).await {
        Err(EmbedError::InstanceInProcess { running }) => assert_eq!(running, "one"),
        Err(other) => panic!("expected InstanceInProcess, got {other:?}"),
        Ok(_) => panic!("a second named instance started in this process"),
    }
    assert!(
        !b.data.join("scenes/code/daemon.lock").exists(),
        "the refused start kept its lock"
    );
    let anonymous = start(&b).await?;
    anonymous.shutdown().await;

    first.shutdown().await;
    let second = Embedded::start(named(&b, "two")).await?;
    second.shutdown().await;
    Ok(())
}

/// One process, one process-group record: an engine with a `groups_file` and
/// no instance holds it as a named instance does.
#[tokio::test(flavor = "multi_thread")]
async fn a_process_keeps_one_process_group_record() -> anyhow::Result<()> {
    let _named = NAMED.lock().await;
    let stub = ProviderStub::start().await?;
    let (a, b) = (root(&stub)?, root(&stub)?);
    let recording = |r: &Root| {
        let mut c = config(r);
        c.groups_file = Some(r.data.join("groups.json"));
        c
    };
    let first = Embedded::start(recording(&a)).await?;
    for second in [recording(&b), {
        let mut c = config(&b);
        c.instance = Some("two".into());
        c
    }] {
        match Embedded::start(second).await {
            Err(EmbedError::GroupsInProcess { running }) => {
                assert_eq!(running, a.data.join("groups.json"))
            }
            Err(other) => panic!("expected GroupsInProcess, got {other:?}"),
            Ok(_) => panic!("a second record was kept in this process"),
        }
    }
    let anonymous = start(&b).await?;
    anonymous.shutdown().await;

    first.shutdown().await;
    assert!(
        !a.data.join("groups.json").exists(),
        "the record outlived shutdown"
    );
    let second = Embedded::start(recording(&b)).await?;
    second.shutdown().await;
    Ok(())
}

/// Set in the host process [`a_killed_hosts_command_is_stopped_by_the_next_start_on_its_groups_file`]
/// starts: the directory of the `Root` it runs on.
const HOST_ROOT: &str = "AGIX_TEST_EMBED_HOST_ROOT";

fn host_groups_file(dir: &Path) -> PathBuf {
    dir.join("host-groups.json")
}

fn recorded_groups(file: &Path) -> Vec<i64> {
    std::fs::read(file)
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|v| v["groups"].as_array().cloned())
        .unwrap_or_default()
        .iter()
        .filter_map(Value::as_i64)
        .collect()
}

/// The host process, killed when the case ends however it ends.
struct Host(std::process::Child);

impl Drop for Host {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn group_alive(group: i64) -> bool {
    std::process::Command::new("kill")
        .args(["-0", "--", &format!("-{group}")])
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// The host: an engine without an instance, recording at the groups file,
/// in a turn whose command never ends. It waits to be killed.
async fn host_a_command_that_never_ends(dir: &Path) -> anyhow::Result<()> {
    let mut c = EmbedConfig::new(dir.join("data"));
    c.cwd = Some(dir.join("project"));
    c.groups_file = Some(host_groups_file(dir));
    let embedded = Embedded::start(c).await?;
    let mut wire = duplex_wire(&embedded);
    let created = wire.call("session.create", json!({})).await?;
    let session = created["result"]["session_id"].clone();
    wire.send(
        "session.run_turn",
        json!({ "session_id": session, "message": "wait a while", "turn_id": "t1" }),
    )
    .await?;
    tokio::time::sleep(Duration::from_secs(120)).await;
    anyhow::bail!("the host was never killed")
}

/// Without an instance, a `groups_file` alone has the next start stop the
/// command a `SIGKILL`ed host left running. The host is this case run again
/// in a process of its own.
#[tokio::test(flavor = "multi_thread")]
async fn a_killed_hosts_command_is_stopped_by_the_next_start_on_its_groups_file(
) -> anyhow::Result<()> {
    if let Some(dir) = std::env::var_os(HOST_ROOT) {
        return host_a_command_that_never_ends(Path::new(&dir)).await;
    }
    let _named = NAMED.lock().await;
    let stub = ProviderStub::start().await?;
    let r = root(&stub)?;
    let dir = r.data.parent().unwrap().to_path_buf();
    let mut settings: Value =
        serde_json::from_slice(&std::fs::read(r.data.join("settings.json"))?)?;
    settings["memory_enabled"] = json!(false);
    settings["permission_rules"] = json!([{ "tool": "Bash", "action": "allow" }]);
    std::fs::write(r.data.join("settings.json"), settings.to_string())?;
    stub.script_when(TITLE_NEEDLE, (0..4).map(|_| Reply::text("a title")));
    stub.script([Reply::calls(
        "call-1",
        "Bash",
        json!({ "command": "tail -f /dev/null", "timeout_ms": 600000 }),
    )]);

    let case = concat!(
        "embed_entry::",
        "a_killed_hosts_command_is_stopped_by_the_next_start_on_its_groups_file"
    );
    let log = dir.join("host.log");
    let mut host = Host(
        std::process::Command::new(std::env::current_exe()?)
            .args([case, "--exact", "--nocapture", "--test-threads=1"])
            .env(HOST_ROOT, &dir)
            .current_dir(&r.project)
            .stdin(std::process::Stdio::null())
            .stdout(std::fs::File::create(&log)?)
            .stderr(std::fs::File::create(dir.join("host.err"))?)
            .spawn()?,
    );
    let file = host_groups_file(&dir);
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let mut seen = Vec::new();
    let group = loop {
        let now = recorded_groups(&file);
        if let Some(g) = now.iter().find(|g| seen.contains(*g) && group_alive(**g)) {
            break *g;
        }
        seen = now;
        let gone = host.0.try_wait()?;
        anyhow::ensure!(
            gone.is_none() && std::time::Instant::now() < deadline,
            "the command never showed up in {} (host {gone:?}):\n{}",
            file.display(),
            std::fs::read_to_string(&log).unwrap_or_default()
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    };
    drop(host);
    assert!(
        group_alive(group),
        "the command died with its host; this case no longer tests anything"
    );

    let mut c = config(&r);
    c.groups_file = Some(file.clone());
    let again = Embedded::start(c).await?;
    let left = group_alive(group);
    if left {
        let _ = std::process::Command::new("kill")
            .args(["-9", "--", &format!("-{group}")])
            .status();
    }
    assert!(
        !left,
        "the next start left the killed host's command running"
    );
    again.shutdown().await;
    assert!(!file.exists(), "the record outlived shutdown");
    Ok(())
}

/// After a recording engine shuts down, a later engine in the same
/// process that does not record must not bring the first one's file back.
#[tokio::test(flavor = "multi_thread")]
async fn a_shut_down_record_is_not_rewritten_by_a_later_engine() -> anyhow::Result<()> {
    let _named = NAMED.lock().await;
    let stub = ProviderStub::start().await?;
    let (a, b) = (root(&stub)?, root(&stub)?);
    let file = a.data.join("groups.json");
    let mut c = config(&a);
    c.groups_file = Some(file.clone());
    let first = Embedded::start(c).await?;
    first.shutdown().await;
    assert!(!file.exists(), "the record outlived shutdown");

    let mut settings: Value =
        serde_json::from_slice(&std::fs::read(b.data.join("settings.json"))?)?;
    settings["memory_enabled"] = json!(false);
    settings["permission_rules"] = json!([{ "tool": "Bash", "action": "allow" }]);
    std::fs::write(b.data.join("settings.json"), settings.to_string())?;
    stub.script_when(TITLE_NEEDLE, (0..4).map(|_| Reply::text("a title")));
    stub.script([
        Reply::calls("call-1", "Bash", json!({ "command": "sleep 1" })),
        Reply::text("done"),
    ]);
    let other = start(&b).await?;
    let mut wire = duplex_wire(&other);
    let created = wire.call("session.create", json!({})).await?;
    let session = created["result"]["session_id"].clone();
    let done = wire
        .call(
            "session.run_turn",
            json!({ "session_id": session, "message": "run it", "turn_id": "t1" }),
        )
        .await?;
    other.shutdown().await;
    let back = file.exists();
    let _ = std::fs::remove_file(&file);
    assert!(
        !back,
        "an engine that keeps no record rewrote a shut-down engine's file ({done})"
    );
    Ok(())
}

/// A groups file whose owner is another live process is that
/// process's list; a start on it is refused and leaves it alone.
#[tokio::test(flavor = "multi_thread")]
async fn a_live_owners_groups_file_is_not_taken_over() -> anyhow::Result<()> {
    let _named = NAMED.lock().await;
    let stub = ProviderStub::start().await?;
    let r = root(&stub)?;
    let file = r.data.join("groups.json");
    let mut other = std::process::Command::new("sleep").arg("30").spawn()?;
    let owner = other.id();
    // The record names a live owner and one group of its own.
    let record = json!({ "owner_pid": owner, "owner_started_at": 0, "groups": [owner] });
    std::fs::write(&file, record.to_string())?;
    let mut c = config(&r);
    c.groups_file = Some(file.clone());
    let refused = Embedded::start(c).await;
    let now: Value = serde_json::from_slice(&std::fs::read(&file)?)?;
    let _ = other.kill();
    let _ = other.wait();
    assert!(
        matches!(refused, Err(EmbedError::GroupsHeld { owner: o, .. }) if o == owner),
        "start on a live owner's record was not refused"
    );
    assert_eq!(
        now["owner_pid"],
        json!(owner),
        "the live owner's record was overwritten: {now}"
    );
    Ok(())
}
