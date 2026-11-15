//! `agixd` — daemon binary.
//!
//! The command line, the runtime, logging, the crash hook, signals and the
//! OpenTelemetry flush are this process's; everything else is
//! `daemon::embed`, the entry an embedder starts the engine through too, plus
//! the TCP / WebSocket transports of `daemon::agixd`.

use std::path::PathBuf;
use std::sync::Arc;

use daemon::embed::{EmbedConfig, Embedded};
use tracing::info;

const HELP: &str = "\
Agix daemon: multi-session agent engine over JSON-RPC

Usage: agixd [OPTIONS]

Options:
      --socket <PATH>              Unix socket path [default: $HOME/.agix/scenes/<scene>/daemon.sock]
      --session-cap <N>            Maximum concurrent sessions [default: 32]
      --session-idle-timeout <SECS>
                                   Session idle timeout in seconds [default: 3600]
      --model <NAME>               Model name for sessions [default: claude-sonnet-4-6]
      --max-tokens <N>             Max tokens per turn [default: 32000]
      --listen <ADDR>              Bind a TCP listener at ADDR (e.g. 127.0.0.1:7878);
                                   requires --token or AGIX_DAEMON_TOKEN
      --listen-ws <ADDR>           Bind a WebSocket listener at a loopback ADDR
                                   (e.g. 127.0.0.1:7879); requires --token or AGIX_DAEMON_TOKEN
      --token <SECRET>             Shared secret for TCP and WebSocket auth [env: AGIX_DAEMON_TOKEN]
      --unix-socket-auth           The unix socket, too, requires daemon.auth: with the
                                   --token / AGIX_DAEMON_TOKEN secret when one is given, else
                                   with a fresh one written to
                                   ~/.agix/daemon/auth/<instance>.token
      --scene <ID>                 Scene this daemon serves; also selects the state root
                                   ~/.agix/scenes/<scene>/ [default: code]
      --scenes <ID,...>            Additional scenes to activate alongside --scene
      --instance <NAME>            Name of this daemon's ~/.agix/daemon/instances.d/ entry
                                   [default: the active scenes, sorted and comma-joined]
      --permission-prompt-timeout <SECS>
                                   Seconds a permission prompt may go unanswered before it is
                                   denied (0 = wait forever)
                                   [default: execution.permission_prompt_timeout_secs, else 300]
      --worker-threads <N>         Async worker threads [env: AGIX_WORKER_THREADS]
                                   [default: the number of CPU cores]
      --mcp-policy <PATH>          MCP policy file to enforce instead of the managed one
                                   [default: /Library/Application Support/Agix/mcp-policy.json
                                   on macOS, %ProgramData%\\Agix\\mcp-policy.json on Windows,
                                   /etc/agix/mcp-policy.json elsewhere]
      --managed-settings <PATH>    Managed settings file to enforce instead of the system one
                                   [default: /Library/Application Support/Agix/managed-settings.json
                                   on macOS, %ProgramData%\\Agix\\managed-settings.json on
                                   Windows, /etc/agix/managed-settings.json elsewhere]
  -h, --help                       Print help
  -V, --version                    Print version
";

#[derive(Debug)]
struct Cli {
    /// Unix socket path (default: $HOME/.agix/scenes/<scene>/daemon.sock)
    socket: Option<PathBuf>,

    /// Maximum concurrent sessions
    session_cap: usize,

    /// Session idle timeout in seconds (default: 3600 = 1 hour)
    session_idle_timeout: u64,

    /// Model name for sessions
    model: String,

    /// Max tokens per turn
    max_tokens: u32,

    /// Bind a TCP listener at this addr (e.g. 127.0.0.1:7878).
    /// Requires --token or AGIX_DAEMON_TOKEN env.
    listen: Option<String>,

    /// Bind a WebSocket listener at this addr (e.g. 127.0.0.1:7879), so a
    /// browser front end can reach the daemon directly. Loopback only, and
    /// requires --token or AGIX_DAEMON_TOKEN env.
    listen_ws: Option<String>,

    /// Shared secret for TCP and WebSocket auth. Falls back to
    /// `AGIX_DAEMON_TOKEN`.
    token: Option<String>,

    /// Which scene this daemon serves (must be one the code actually
    /// registers — see `resolve_scene()`). Determines both session behavior
    /// (system prompt/tools) and the user-level state root (`~/.agix/scenes/<scene>/`).
    /// There is no default scope concept separate from this — an unsupported
    /// value fails startup outright rather than silently falling back to
    /// anything.
    scene: String,

    /// Additional scenes to activate alongside `--scene`, comma-separated
    /// (e.g. `--scenes harnix,mini`). `--scene`'s own value doesn't need
    /// repeating here — it's always active. Each must be one
    /// `scene::SceneRegistry::register_builtin` actually registers;
    /// an unknown name fails startup the same way an unsupported `--scene`
    /// does. Session creation still defaults to `--scene`'s scene;
    /// `session.create {"scene": "..."}` is how a caller reaches one of
    /// these instead.
    scenes: Vec<String>,

    /// Named identity for this daemon instance — the filename of its
    /// `~/.agix/daemon/instances.d/<instance>.json` discovery entry
    /// (`docs/daemon_rpc_protocol.md` §2). Must be unique among daemons running on
    /// the same machine; two live instances racing to write the same name
    /// isn't reconciled here (each just overwrites the file — see
    /// `discovery::write_instance_file`), so operators are expected to pick
    /// distinct names when running more than one daemon.
    ///
    /// Default (when omitted): every active scene (`--scene` plus
    /// `--scenes`), deduplicated, sorted, and comma-joined — e.g.
    /// `--scene code --scenes harnix,mini` defaults to
    /// `code,harnix,mini`. See `default_instance_name()`.
    instance: Option<String>,

    /// Seconds a permission prompt may go unanswered before the daemon
    /// denies it on the client's behalf (0 = wait forever).
    ///
    /// The engine itself waits indefinitely for `session.respondToPrompt`, so a
    /// client that never implements it would otherwise hang its
    /// `session.run_turn` call forever. The timeout fails *closed* — it
    /// denies the tool call, never approves it — and the turn then continues
    /// normally with an error `tool_result`.
    permission_prompt_timeout: Option<u64>,

    /// Async worker threads; `None` leaves tokio's default of one per CPU
    /// core. Blocking work (file I/O, searches, scripts) runs on its own pool
    /// whatever this is.
    worker_threads: Option<usize>,

    /// The MCP policy file, in place of the platform's managed path. Unlike
    /// that one it must exist: a policy someone named and that is not there
    /// denies every MCP server rather than none.
    mcp_policy: Option<PathBuf>,

    /// The managed settings file, in place of the platform's. It must exist:
    /// one someone named and that is not there refuses every tool call.
    managed_settings: Option<PathBuf>,

    /// The unix socket requires `daemon.auth` as TCP does.
    unix_socket_auth: bool,
}

fn number<T: std::str::FromStr>(parser: &mut lexopt::Parser, flag: &str) -> Result<T, lexopt::Error>
where
    T::Err: std::fmt::Display,
{
    use lexopt::prelude::*;
    let value = parser.value()?.string()?;
    value
        .parse()
        .map_err(|e| format!("invalid value '{value}' for '--{flag}': {e}").into())
}

enum Invocation {
    Run(Box<Cli>),
    Help,
    Version,
}

impl Cli {
    fn parse() -> Self {
        let parsed = Self::parse_from(std::env::args_os().skip(1)).and_then(|invocation| {
            let Invocation::Run(mut cli) = invocation else {
                return Ok(invocation);
            };
            if cli.worker_threads.is_none() {
                if let Some(value) = std::env::var_os("AGIX_WORKER_THREADS") {
                    cli.worker_threads = Some(worker_threads(&value.to_string_lossy())?);
                }
            }
            Ok(Invocation::Run(cli))
        });
        match parsed {
            Ok(Invocation::Run(cli)) => *cli,
            Ok(Invocation::Help) => {
                print!("{HELP}");
                std::process::exit(0)
            }
            Ok(Invocation::Version) => {
                println!("agixd {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0)
            }
            Err(e) => {
                eprintln!("error: {e}\n\nFor more information, try '--help'.");
                std::process::exit(2)
            }
        }
    }

    fn parse_from(
        args: impl IntoIterator<Item = std::ffi::OsString>,
    ) -> Result<Invocation, lexopt::Error> {
        use lexopt::prelude::*;
        let mut cli = Cli {
            socket: None,
            session_cap: 32,
            session_idle_timeout: 3600,
            model: "claude-sonnet-4-6".into(),
            max_tokens: settings::DEFAULT_MAX_TOKENS,
            listen: None,
            listen_ws: None,
            token: None,
            scene: "code".into(),
            scenes: Vec::new(),
            instance: None,
            permission_prompt_timeout: None,
            worker_threads: None,
            mcp_policy: None,
            managed_settings: None,
            unix_socket_auth: false,
        };
        let mut parser = lexopt::Parser::from_args(args);
        while let Some(arg) = parser.next()? {
            match arg {
                Long("socket") => cli.socket = Some(parser.value()?.into()),
                Long("session-cap") => cli.session_cap = number(&mut parser, "session-cap")?,
                Long("session-idle-timeout") => {
                    cli.session_idle_timeout = number(&mut parser, "session-idle-timeout")?
                }
                Long("model") => cli.model = parser.value()?.string()?,
                Long("max-tokens") => cli.max_tokens = number(&mut parser, "max-tokens")?,
                Long("listen") => cli.listen = Some(parser.value()?.string()?),
                Long("listen-ws") => cli.listen_ws = Some(parser.value()?.string()?),
                Long("token") => cli.token = Some(parser.value()?.string()?),
                Long("scene") => cli.scene = parser.value()?.string()?,
                Long("scenes") => cli.scenes.extend(
                    parser
                        .value()?
                        .string()?
                        .split(',')
                        .filter(|s| !s.is_empty())
                        .map(str::to_string),
                ),
                Long("instance") => cli.instance = Some(parser.value()?.string()?),
                Long("permission-prompt-timeout") => {
                    cli.permission_prompt_timeout =
                        Some(number(&mut parser, "permission-prompt-timeout")?)
                }
                Long("worker-threads") => {
                    cli.worker_threads = Some(worker_threads(&parser.value()?.string()?)?)
                }
                Long("mcp-policy") => cli.mcp_policy = Some(parser.value()?.into()),
                Long("managed-settings") => cli.managed_settings = Some(parser.value()?.into()),
                Long("unix-socket-auth") => cli.unix_socket_auth = true,
                Short('h') | Long("help") => return Ok(Invocation::Help),
                Short('V') | Long("version") => return Ok(Invocation::Version),
                _ => return Err(arg.unexpected()),
            }
        }
        Ok(Invocation::Run(Box::new(cli)))
    }
}

fn worker_threads(value: &str) -> Result<usize, lexopt::Error> {
    match value.parse::<usize>() {
        Ok(n) if n > 0 => Ok(n),
        _ => Err(format!(
            "invalid worker thread count '{value}': expected an integer of at least 1"
        )
        .into()),
    }
}

fn runtime(worker_threads: Option<usize>) -> std::io::Result<tokio::runtime::Runtime> {
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    if let Some(n) = worker_threads {
        builder.worker_threads(n);
    }
    builder.enable_all().build()
}

/// Every scene this daemon activates at startup — `--scene` plus
/// `--scenes`, deduplicated and sorted. Sorting makes this (and anything
/// derived from it, like [`default_instance_name`]) independent of the
/// order `--scenes` happened to list them in.
fn active_scenes(scene: &str, scenes: &[String]) -> Vec<String> {
    let mut all: Vec<String> = std::iter::once(scene.to_string())
        .chain(scenes.iter().cloned())
        .collect();
    all.sort();
    all.dedup();
    all
}

/// Default `--instance` name: the active scene set, sorted and joined with
/// `,` — `docs/daemon_rpc_protocol.md` §2 specifies "排序后连接" for the analogous
/// default socket name without pinning a delimiter; comma matches
/// `--scenes`' own list syntax.
fn default_instance_name(scene: &str, scenes: &[String]) -> String {
    active_scenes(scene, scenes).join(",")
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    runtime(cli.worker_threads)?.block_on(serve(cli))
}

async fn serve(cli: Cli) -> anyhow::Result<()> {
    init_logging();
    let data_root = daemon::config::default_global_root();
    let listeners = listeners_from_cli(&cli)?;
    platform::crash::install(
        data_root.join("daemon").join("crashes"),
        "agixd",
        env!("CARGO_PKG_VERSION"),
    );
    let embedded = Arc::new(Embedded::start(embed_config(&cli, data_root)).await?);
    spawn_shutdown_handler(embedded.clone());
    let served = daemon::agixd::serve(&embedded, listeners).await;
    embedded.shutdown().await;
    #[cfg(feature = "otel")]
    let _ = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tokio::task::spawn_blocking(assembly::telemetry_otel::shutdown_process_exporter),
    )
    .await;
    served?;
    info!("daemon stopped");
    Ok(())
}

fn embed_config(cli: &Cli, data_root: PathBuf) -> EmbedConfig {
    let mut config = EmbedConfig::new(data_root);
    config.cwd = Some(std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    config.scene = cli.scene.clone();
    config.scenes = cli.scenes.clone();
    config.model = cli.model.clone();
    config.max_tokens = cli.max_tokens;
    config.session_cap = cli.session_cap;
    config.session_idle_timeout = std::time::Duration::from_secs(cli.session_idle_timeout);
    config.permission_prompt_timeout = cli
        .permission_prompt_timeout
        .map(std::time::Duration::from_secs);
    config.instance = Some(
        cli.instance
            .clone()
            .unwrap_or_else(|| default_instance_name(&cli.scene, &cli.scenes)),
    );
    config.socket = cli.socket.clone();
    config.mcp_policy = cli.mcp_policy.clone();
    config.managed_settings = cli.managed_settings.clone();
    config
}

/// `--listen`, `--listen-ws`, `--token` / `AGIX_DAEMON_TOKEN` and
/// `--unix-socket-auth`, checked.
fn listeners_from_cli(cli: &Cli) -> anyhow::Result<daemon::agixd::Listeners> {
    let token = cli
        .token
        .clone()
        .or_else(|| std::env::var("AGIX_DAEMON_TOKEN").ok());
    let tcp = match &cli.listen {
        Some(addr_str) => {
            let addr: std::net::SocketAddr = addr_str
                .parse()
                .map_err(|e| anyhow::anyhow!("invalid --listen `{addr_str}`: {e}"))?;
            if token.is_none() {
                anyhow::bail!("--listen requires --token or env AGIX_DAEMON_TOKEN");
            }
            if !addr.ip().is_loopback() {
                tracing::warn!(
                    %addr,
                    "--listen binds an address reachable from the network; the connection is not \
                     encrypted, so the token and everything after it travel in the clear"
                );
            }
            Some(addr)
        }
        None => None,
    };
    #[cfg(not(feature = "ws"))]
    if cli.listen_ws.is_some() {
        anyhow::bail!("--listen-ws: this build has no WebSocket listener (feature `ws`)");
    }
    let ws = match &cli.listen_ws {
        Some(addr_str) => {
            let addr: std::net::SocketAddr = addr_str
                .parse()
                .map_err(|e| anyhow::anyhow!("invalid --listen-ws `{addr_str}`: {e}"))?;
            // Refused rather than warned about: this transport is what a browser
            // connects to, and a browser reaching it from another machine would
            // mean the token is the only thing between the daemon and the network.
            if !addr.ip().is_loopback() {
                anyhow::bail!(
                    "--listen-ws must bind a loopback address; `{addr}` is reachable from the network"
                );
            }
            if token.is_none() {
                anyhow::bail!("--listen-ws requires --token or env AGIX_DAEMON_TOKEN");
            }
            Some(addr)
        }
        None => None,
    };
    Ok(daemon::agixd::Listeners::new(
        tcp,
        ws,
        token,
        cli.unix_socket_auth,
    ))
}

fn init_logging() {
    tracing_subscriber::fmt()
        .with_env_filter(assembly::logging::log_filter(
            std::env::var("RUST_LOG").ok().as_deref(),
        ))
        .init();
}

/// Ctrl-C, and the SIGTERM a supervisor stops a service with, shut down
/// gracefully; a second one exits at once.
fn spawn_shutdown_handler(embedded: Arc<Embedded>) {
    tokio::spawn(async move {
        shutdown_signal().await;
        tokio::spawn(async {
            shutdown_signal().await;
            std::process::exit(130);
        });
        embedded.shutdown().await;
    });
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        if let Ok(mut term) = signal(SignalKind::terminate()) {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => info!("ctrl-c received; shutting down"),
                _ = term.recv() => info!("SIGTERM received; shutting down"),
            }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
    info!("ctrl-c received; shutting down");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_instance_name_sorts_and_dedupes_scenes() {
        assert_eq!(
            default_instance_name("mini", &["harnix".to_string(), "code".to_string()]),
            "code,harnix,mini"
        );
        assert_eq!(
            default_instance_name("code", &["code".to_string()]),
            "code",
            "--scene's own value repeated in --scenes must not appear twice"
        );
    }

    fn parse(args: &[&str]) -> Cli {
        match Cli::parse_from(args.iter().map(std::ffi::OsString::from)) {
            Ok(Invocation::Run(cli)) => *cli,
            Ok(_) => panic!("expected a run invocation for {args:?}"),
            Err(e) => panic!("{args:?}: {e}"),
        }
    }

    #[test]
    fn no_arguments_give_the_documented_defaults() {
        let cli = parse(&[]);
        assert_eq!(cli.session_cap, 32);
        assert_eq!(cli.session_idle_timeout, 3600);
        assert_eq!(cli.model, "claude-sonnet-4-6");
        assert_eq!(cli.max_tokens, 32_000);
        assert_eq!(cli.scene, "code");
        assert_eq!(cli.permission_prompt_timeout, None);
        assert!(cli.socket.is_none() && cli.instance.is_none() && cli.scenes.is_empty());
        assert_eq!(cli.worker_threads, None);
        assert_eq!(cli.mcp_policy, None);
        assert_eq!(cli.managed_settings, None);
        assert!(!cli.unix_socket_auth);
        assert!(parse(&["--unix-socket-auth"]).unix_socket_auth);
    }

    #[test]
    fn the_command_line_defaults_are_the_embed_entrys() {
        let root = PathBuf::from("/data");
        let from_cli = embed_config(&parse(&[]), root.clone());
        let mut comparable = from_cli.clone();
        comparable.cwd = None;
        comparable.instance = None;
        assert_eq!(
            format!("{comparable:?}"),
            format!("{:?}", EmbedConfig::new(root))
        );
        assert_eq!(from_cli.instance.as_deref(), Some("code"));
    }

    #[test]
    fn worker_threads_take_a_positive_count_and_size_the_runtime() {
        assert_eq!(parse(&["--worker-threads", "1"]).worker_threads, Some(1));
        assert_eq!(parse(&["--worker-threads=6"]).worker_threads, Some(6));
        let parse_err =
            |args: &[&str]| Cli::parse_from(args.iter().map(std::ffi::OsString::from)).is_err();
        assert!(parse_err(&["--worker-threads", "0"]));
        assert!(parse_err(&["--worker-threads", "-2"]));
        assert!(parse_err(&["--worker-threads", "all"]));

        let rt = runtime(Some(1)).unwrap();
        assert_eq!(rt.metrics().num_workers(), 1);
        let rt = runtime(Some(3)).unwrap();
        assert_eq!(rt.metrics().num_workers(), 3);
    }

    #[test]
    fn options_take_separate_or_inline_values() {
        let cli = parse(&[
            "--socket",
            "/tmp/a.sock",
            "--session-cap=4",
            "--scene",
            "mini",
            "--scenes",
            "harnix,triage",
            "--scenes=code",
            "--listen",
            "127.0.0.1:7878",
            "--token=s",
            "--mcp-policy",
            "/etc/corp/mcp.json",
            "--managed-settings=/etc/corp/managed.json",
        ]);
        assert_eq!(
            cli.socket.as_deref(),
            Some(std::path::Path::new("/tmp/a.sock"))
        );
        assert_eq!(cli.session_cap, 4);
        assert_eq!(cli.scene, "mini");
        assert_eq!(cli.scenes, ["harnix", "triage", "code"]);
        assert_eq!(cli.listen.as_deref(), Some("127.0.0.1:7878"));
        assert_eq!(cli.token.as_deref(), Some("s"));
        assert_eq!(
            cli.mcp_policy.as_deref(),
            Some(std::path::Path::new("/etc/corp/mcp.json"))
        );
        assert_eq!(
            cli.managed_settings.as_deref(),
            Some(std::path::Path::new("/etc/corp/managed.json"))
        );
    }

    #[test]
    fn unknown_flags_and_bad_numbers_are_errors() {
        let parse_err =
            |args: &[&str]| Cli::parse_from(args.iter().map(std::ffi::OsString::from)).is_err();
        assert!(parse_err(&["--bogus"]));
        assert!(parse_err(&["--session-cap", "many"]));
        assert!(parse_err(&["--model"]));
        assert!(parse_err(&["stray"]));
    }

    #[test]
    fn help_and_version_short_circuit() {
        let run = |args: &[&str]| Cli::parse_from(args.iter().map(std::ffi::OsString::from));
        assert!(matches!(run(&["--model", "x", "-h"]), Ok(Invocation::Help)));
        assert!(matches!(run(&["--version"]), Ok(Invocation::Version)));
    }

    #[test]
    fn default_instance_name_is_order_independent() {
        assert_eq!(
            default_instance_name("harnix", &["mini".to_string(), "code".to_string()]),
            default_instance_name("code", &["mini".to_string(), "harnix".to_string()]),
        );
    }
}
