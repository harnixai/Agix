# Agix

[![CI](https://github.com/openatta/agix/actions/workflows/ci.yml/badge.svg)](https://github.com/openatta/agix/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/openatta/agix?display_name=tag&sort=semver)](https://github.com/openatta/agix/releases)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](#license)
[![Rust](https://img.shields.io/badge/rust-1.88%2B-orange.svg)](#build-and-test)

**中文版:[docs/README.zh-CN.md](docs/README.zh-CN.md)**

**An agent engine you build products on, not an assistant you talk to.**

---

## Purpose

Agix is the engine underneath an agent product: it runs a *session* — assembles
the prompt, calls the model, executes tools, keeps the context within budget,
writes the history, delegates to sub-agents — and leaves every decision along
the way open to the product built on top of it. There is no user interface
here; that is the host's job.

It is meant for people who build on it:

- **A Rust application** links the engine as a library and drives an `Agent`
  directly.
- **Anything else** — an IDE plugin, a web front end, an orchestrator in another
  language — runs `agixd`, the reference host in this repository, and speaks
  JSON-RPC to it over a Unix socket, TCP or WebSocket.

Both are the same engine.

---

## Why Agix

**One engine for a rack of servers and a board on the shelf.** Agix is built
to be deployed by the hundred and embedded where every megabyte counts: a
single small binary, a predictable footprint, and a loop that keeps working
when no one is watching.

1. **Small enough to run anywhere.** `agixd` is one 11 MB binary with no
   runtime to install. It idles at about 10 MB of memory and starts in about
   30 ms. A real task — four sub-agents in parallel plus a program, 91 model
   calls over six minutes — peaked under 30 MB and used about 8 s of CPU (measured
   on an M2 Max, `release` build). Size the async workers to the hardware
   with `--worker-threads` — one is enough on a small board — and leave out
   whole modules at build time with the build tiers.
2. **Many sessions, one process.** A daemon serves dozens of sessions across
   projects and scenes, over a Unix socket, TCP or WebSocket. Idle sessions
   are evicted and come back from disk on demand. A crash is recorded,
   leftover commands are cleaned up on the next start, and the daemon runs
   under systemd or launchd like any other service.
3. **Safe to leave alone.** Mark a scene unattended and nothing ever waits on
   a human: what would need approval is refused with a reason, writes stay
   inside the roots you gave it, and `Bash` runs only inside the OS sandbox
   (Seatbelt on macOS, bubblewrap on Linux). Authority comes from settings,
   never from task text, so injected content cannot grant itself anything.
4. **More work per token.** With `RunCode` the model drives tools from a
   short program — in parallel, in loops, across sub-agents whose answers
   are checked against a JSON Schema — and only the result comes back. Goals
   keep a session going round after round until the work is done. The fixed
   prompt prefix is about 5.8K tokens, shared across sessions by the
   provider's cache, and long sessions compact themselves and carry on. On
   six engineering tasks, against another open agent harness on the same
   model, that came to **34% less wall time and 13% fewer prompt tokens, with
   every run correct** (the other harness's wall time includes its process
   start, while the Agix daemon is already running; [how it was measured](tests/blackbox/tests/compare_dsh.rs)).
5. **Your product stays in charge.** Every session, task, sub-agent, goal and
   prompt is visible, stoppable and editable over one JSON-RPC protocol.
   46 extension points let you replace or intercept any subsystem from Rust
   or a QuickJS script without forking. Every session is an
   append-only log you can resume, fork, export or replay byte for byte
   with no network.

---

## Features

**Deployment**
- `agixd`: a daemon speaking JSON-RPC 2.0 (NDJSON) over a Unix socket, TCP
  or WebSocket, with token authentication off the local socket.
- Or embed the engine: one `Agent` from the builder, or the daemon's whole
  session pool in your own process.
- `release` builds an 11 MB binary; `release-fast` trades twice the size for
  a little speed and unwinding on panic.
- Build tiers (`minimal`, `coding`, `full`) and per-module features: MCP,
  sub-agents, skills, web tools, structured output, `RunCode`, WebSocket,
  scripts, plugins, the sandbox, OpenTelemetry.
- `--worker-threads` / `AGIX_WORKER_THREADS`; session cap with least-recently-used
  eviction; idle timeout; several scenes per process.
- Discovery files, so clients find a daemon by scene. Clean shutdown on
  SIGTERM, crash records, and process-group cleanup after a hard kill.

**Models**
- Anthropic and OpenAI-compatible APIs, so hosted and local models
  (llama.cpp, Ollama, vLLM, …) both work.
- Per-task routing (`main`, `subagent`, `compact`, `memory`, `web_search`,
  `web_fetch`),
  per-session providers, and model switching mid-session.
- Retry with backoff, plus recovery: switch model, compact and retry, or
  raise `max_tokens`.
- A cache-friendly prompt: per-session facts ride in the first message,
  rarely used tools are loaded on demand with `ToolSearch`, and compaction
  emits cache edits.

**Autonomy**
- Goals that run automatic rounds until done, blocked or out of budget.
- Cron-style schedules, plan mode with verification, and todo lists.
- Background shell commands, monitors, sub-agents and programs, each
  reporting back with a notification.
- `RunCode`: a JavaScript program (embedded QuickJS) that calls the
  session's tools, sub-agents included.

**Multi-agent**
- The `Agent` tool with typed agents: foreground or background, with their
  own models and tools, and results validated against a JSON Schema.
- Teams with a mailbox and shared memory.
- Sub-agents run in sidechain sessions linked to their parent.
- Concurrency and depth limits, and permissions, hooks and budgets that
  follow delegation.

**Tools** — 45 built in: files, shell, web, tasks, planning, goals,
scheduling, git worktrees, skills, teams, MCP, questions, notifications,
monitors and structured output ([table below](#tools--45-built-in)).

**Context**
- An LLM summary first, then cheaper fallbacks, behind a predictive
  trigger, a circuit breaker and a configurable threshold.
- Session memory with recall.
- A skill catalog.
- Token and output budgets.

**Safety**
- A fixed permission order: credentials are refused first, in every mode; then
  the tool's own check, then rules (a deny always wins), then path safety, then
  the mode.
- Modes `default`, `acceptEdits`, `plan`, `dontAsk` and `bypassPermissions`.
- An OS sandbox for shell commands, with git hooks, git config and agent
  settings read-only inside it; the engine's own git runs there too.
- Unattended scenes that refuse instead of asking.
- Permission prompts that time out closed.
- Worktree isolation.
- Workspace trust: a project the user has not trusted gets plugin-level
  authority — its own settings can only tighten, and its hooks, MCP servers,
  skill commands and grants, looser agent types, and the settings that pick
  where data goes (providers, endpoints, the instruction file) wait for trust
  (granted in the global tier or over RPC; untrusted by default,
  [ARCHITECTURE.md](docs/ARCHITECTURE.md) §6.6).

**Host control**
- Seventy-four RPC methods: create, resume, fork, rebuild, rewind, export and import sessions;
  read history and the exact context of the next model call; answer
  permission and question prompts.
- Start, message, stop and follow tasks, reading their output by byte offset.
- Switch the model or prompt; manage scenes, MCP servers, plugins,
  schedules and which projects are trusted.

**Extensibility**
- 46 published extension points: 31 contracts, 9 interceptions and 6
  registrations.
- Rust plugins compiled into the host, and QuickJS scripts, installable as
  packages with install-time disclosure.
- Hooks at twenty-nine moments, with command, prompt, HTTP and agent backends
  ([`docs/hooks.md`](docs/hooks.md)).
- An MCP client (stdio, SSE, streamable HTTP, WebSocket) on MCP 2026-07-28,
  falling back to `initialize` for 2025-11-25 servers; what a server asks the
  user mid-call reaches the host as a prompt. Declarative scenes and custom
  agent types.
- MCP authorization (the 2026-07-28 spec, OAuth 2.1): discovery, client ID
  metadata documents or dynamic client registration, PKCE, the RFC 9207 `iss`
  check, refresh; sign-in driven by the host over RPC with
  a loopback or host-owned redirect; client credentials and enterprise-managed
  authorization for unattended servers; tokens in 0600 files or the host's own
  store ([`docs/mcp_auth.md`](docs/mcp_auth.md)).
- An enterprise MCP policy: allow and deny lists by server name, command or
  URL, read from a system path users cannot write (or `--mcp-policy`); an
  unreadable policy denies every server
  ([ARCHITECTURE.md](docs/ARCHITECTURE.md) §6.4).
- Managed settings for the deployment, read from a system path users cannot
  write (or `--managed-settings`): their deny and ask rules hold even under
  `bypassPermissions`, and they can switch that mode off; an unreadable file
  refuses every tool call ([ARCHITECTURE.md](docs/ARCHITECTURE.md) §6.1).

**Records**
- An append-only JSONL history that can be resumed, forked or rebuilt after
  a crash.
- 22 telemetry event types, with OpenTelemetry export (metrics and a span tree)
  ([`docs/telemetry.md`](docs/telemetry.md)).
- A recorder for byte-exact offline replay.
- Session export as a zip archive or a directory, sub-agents included.

---

## Architecture

A Rust workspace in layers. Dependencies point downward only, and a test
(`tests/guards/tests/deps.rs`) fails the build if one points the other way.

```
 shell      daemon ─────────── agixd: command line, process lifecycle
 assembly   assembly ───────── session pool, (project, scene) bindings, provider routing,
              │                the module list; the JSON-RPC server and discovery
 modules    agents (+team) · mcp · skills · script-host · packages
 kernel     runtime (K3) → hooks · tools · session · compaction (K2)
              → model · scene · history · telemetry · permissions · ptc ·
                settings · memory · skill-format (K1)
 machine    platform ───────── this machine's process, filesystem, network and OS sandbox
 contract   core (crate `base`) — the traits, shared data types and pure decisions
```

Kernel crates never depend on a module crate, and only `assembly` and `daemon`
know which modules exist. A turn is a fixed skeleton of steps with an open
decision at each one; a turn has exactly one exit, so `TurnComplete` means
"idle, and on disk". Every tool call, whether the model's, a `RunCode`
sub-call or a skill's inline command, goes through one pipeline. History is an
append-only log that projects into the messages the model sees. Every agent is
its own task and they talk through channels, never shared state. Tools reach
the machine through four contracts (`Process`, `FileSystem`, `Network`,
`Sandbox`), and permissions run in a fixed order in which a deny always wins.
[`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) is the design: the turn, sessions
and scenes, the history log, execution, permissions and trust, locks, disk
layout.

### The extension surface

The engine publishes a catalog of its seams, generated from the code: **46
extension points** — 31 *contracts* (replace a subsystem), 9 *interceptions* (sit
in the path of something in flight), 6 *registrations* (contribute to a
collection). From Rust, implement the trait and hand it to the builder. From
outside a Rust build:

| Route | Reaches | Cost |
|---|---|---|
| **Scripts** (QuickJS, default) | nine points: four that write the prompt, two around a tool call, two around a model call, memory recall | a `.js` file and one line of config; microseconds per call |
| **Plugin packages** | MCP servers, scripts, a declarative scene, agent types, skills | a zip with a `plugin.toml`; everything it will show the model is disclosed at install |
| **Hooks** | Twenty-nine named moments in a turn (`HookEvent` (29)), four backends: command, prompt, HTTP, agent | observe, rewrite an input, block a tool call, end a turn |

And anything that speaks MCP is already an extension. Scripts run in the same
embedded QuickJS as `RunCode`: their limits bound cost, not hostile code, so
what a script may do follows from where it came from. There is no
WebAssembly runtime; a host that wants one builds it on the Rust interfaces.

### Build tiers

| tier | what it assembles |
|---|---|
| `minimal` (`--no-default-features`) | the kernel only: models, the turn loop, scenes, sessions, telemetry, hooks, file and shell tools |
| `coding` | + MCP, sub-agents and teams, skills, the web tools, structured output, programmatic tool calling (`RunCode`) |
| `full` (default, with `scripts`, `plugin-packages` and `sandbox`) | + the WebSocket listener |

`daemon.ping` reports the modules a build carries; a method belonging to one it
does not carry answers `METHOD_NOT_FOUND` naming the module.

### Crate map

| Layer | Crate | Responsibility |
|---|---|---|
| contract | `core` (`base`) | traits, messages, prompt types, the extension catalog, pure decisions |
| machine | `platform` | the local process, filesystem, network and OS sandbox behind the execution contracts |
| kernel | `runtime` | turn loop, streaming tool executor, `Builder`, `Module` |
| kernel | `model` | Anthropic Messages, OpenAI Chat Completions and Responses adapters, retry and backoff |
| kernel | `tools` | built-in tools (`web` and `structured-output` are features) |
| kernel | `scene` | built-in scenes, `SceneRegistry`, declarative scenes |
| kernel | `history` · `session` · `compaction` | transcript log, session state, context compaction |
| kernel | `permissions` · `hooks` · `telemetry` | rule engine, lifecycle hooks, telemetry and the recorder |
| kernel | `settings` · `memory` · `skill-format` | settings tiers and project trust, file-based memory, `SKILL.md` and `AGENTS.md` readers |
| kernel | `ptc` | `RunCode`: the model's programs, run in an embedded QuickJS ([`docs/ptc.md`](docs/ptc.md)) |
| module | `agents` · `team` | the `Agent` tool, agent types, teams |
| module | `mcp` | MCP client and tool adapters, MCP OAuth authorization |
| module | `skills` | skill catalog and the `Skill` tool |
| module | `script-host` | the QuickJS carrier |
| module | `packages` | plugin packages: manifest, install, cache, disclosure |
| assembly | `assembly` | session pool, configuration, provider routing, the JSON-RPC server |
| shell | `daemon` | the `agixd` binary |

23 crates under `crates/`, plus `daemon/`, the seven test members and the
example plugin in `examples/plugins/rust` — 32 workspace members in total.

---

## Quick start

### Run the daemon

```sh
git clone https://github.com/openatta/agix && cd agix
export ANTHROPIC_AUTH_TOKEN=sk-...        # or ANTHROPIC_API_KEY; ANTHROPIC_BASE_URL for a compatible endpoint
cargo run --release -p daemon --bin agixd # listens on ~/.agix/scenes/code/daemon.sock
```

Talk to it — one JSON object per line:

```sh
sock=$HOME/.agix/scenes/code/daemon.sock
echo '{"jsonrpc":"2.0","id":1,"method":"session.create","params":{"project_root":"'$PWD'"}}' \
  | socat - UNIX-CONNECT:$sock
# → {"jsonrpc":"2.0","id":1,"result":{"session_id":"…",…}}

echo '{"jsonrpc":"2.0","id":2,"method":"session.run_turn",
       "params":{"session_id":"<id>","message":"List the crates in this workspace"}}' \
  | socat - UNIX-CONNECT:$sock
```

While the turn runs, `session.event` frames stream back (text deltas, tool
calls, tool results, prompts, task progress); the response arrives when the turn
ends. A typical client subscribes to a session on one connection
(`session.subscribe`) and drives it from another. Seventy-four methods are
documented in [`docs/daemon_rpc_protocol.md`](docs/daemon_rpc_protocol.md) —
sessions, history and context, tasks and sub-agents, scenes, configuration,
project trust, MCP, plugins, scheduling — and every documented response shape is compared
against a real daemon's answer by a test.

Useful flags: `--scene` / `--scenes` (which scenes to serve), `--listen` /
`--listen-ws` with `--token` (TCP / WebSocket, authenticated), `--unix-socket-auth` (the unix socket authenticates too), `--session-cap`,
`--worker-threads`, `--permission-prompt-timeout`, `--mcp-policy`, `--managed-settings`. Each running daemon writes a discovery file
under `~/.agix/daemon/instances.d/`, so a client finds it by scene instead of
by path.

### Embed the engine

As a library — one agent, driven directly:

```rust
let (mut agent, mut events, _input) = runtime::agent::Builder::new()
    .scene(Arc::new(scene::code::spec(Interaction::Interactive)))
    .model(Arc::new(model::adapter::AnthropicModel::new(client)))
    .settings(Arc::new(settings))
    .build()?;

tokio::spawn(async move {
    while let Some(event) = events.recv().await {
        if let AgentEvent::TextDelta { text, .. } = event { print!("{text}"); }
    }
});
agent.run_turn("List the files here".into(), "turn-1".into(), CancellationToken::new()).await?;
```

A scene, a model and settings are required; everything else has a working
default, and every default is a trait you can replace. The complete program is
[`crates/runtime/examples/minimal.rs`](crates/runtime/examples/minimal.rs):

```sh
ANTHROPIC_API_KEY=... cargo run --release -p runtime --example minimal -- "list the files here"
```

As a session pool — the daemon's engine in your own process, with the same
settings tiers, routing, history layout and modules, and your own scenes:

```rust
let pool = daemon::assemble::pool(
    &load_daemon_config("claude-sonnet-4-6", 8192, None, "code", &paths),
    Arc::new(scene::code::spec(Interaction::Interactive)),
    daemon::Assembly { scenes: vec![my_scene], ..Default::default() },
)
.await?;
```

`agixd` is this call plus a command line and its listeners; a test fails the
build if anything in the workspace assembles a pool a second way.

### Extend it

- A new behavior for one point → a script: a `.js` file and one line in
  `settings.json` ([`docs/extending_quickjs.md`](docs/extending_quickjs.md)).
- A packaged capability — MCP servers, scripts, a scene, agent types — → a
  plugin package ([`docs/plugin_packages.md`](docs/plugin_packages.md)).
- Tools, interceptors or prompt contributions in your own build → a Rust
  plugin (`runtime::plugin::Plugin`), added to the builder or to every session
  through `Assembly`. Where a Rust plugin and a script share a point, Rust runs
  first and the script has the last word.
  [`docs/plugin_development.md`](docs/plugin_development.md) walks through both
  kinds, with a working example of each in `examples/plugins/`.
- A different subsystem → implement its trait
  ([`docs/extension_points.md`](docs/extension_points.md) lists every one, with
  what it costs and who may use it).

---

## Capabilities

### Tools — 45 built in

| Category | Tools |
|---|---|
| **Filesystem** | `Read` `Write` `Edit` `Glob` `Grep` |
| **Shell** | `Bash` (sandboxed, path-safe, timeout-controlled) |
| **Web** | `WebFetch` `WebSearch` |
| **Task** (running sub-agents, shell commands, monitors) | `TaskOutput` `TaskWait` `TaskStop` `TaskMessage` |
| **Planning** | `EnterPlanMode` `ExitPlanMode` `TodoWrite` |
| **Goals** | `GoalCreate` `GoalGet` `GoalUpdate` (a goal keeps the session working across automatic rounds) |
| **Scheduling** | `CronCreate` `CronDelete` `CronList` |
| **Workspace** | `EnterWorktree` `ExitWorktree` (isolated git worktree per session) |
| **Collaboration** | `Skill` `Agent` `TeamCreate` `TeamList` `TeamDelete` |
| **Team members** (given only to a team's members) | `SendMessage` `ReadMail` `ListPeers` `TeamMemorySave` `TeamMemoryRead` |
| **Protocol** | `MCP` `ListMcpResources` `ReadMcpResource`, plus one `mcp__<server>__<tool>` per discovered MCP tool |
| **Interaction** | `AskUserQuestion` `PushNotification` `Monitor` `StructuredOutput` |
| **Programs** | `RunCode` (a JavaScript program that calls the session's tools; only its return value comes back) |
| **Editor / Meta / Diagnostics** | `NotebookEdit` · `ToolSearch` · `Ping` `Sleep` |

`tools::register_builtin_tools` installs the 21 self-contained ones; the rest
need per-session state and are registered by the builder or by a module.
Deferred tools are advertised by name and fetched with `ToolSearch` when needed.
The `code` scene leaves out `NotebookEdit`, the worktree tools, `Ping`, `Sleep`
and `PushNotification`; `StructuredOutput` is there only
when the host sets an output schema. A scene is interactive (someone answers
prompts) or unattended: `harnix` and `mini` are unattended, `code` is created
either way. An unattended session has no `AskUserQuestion` or plan mode, refuses
what would need approval instead of asking, and runs `Bash` only inside the OS
sandbox — see `docs/ARCHITECTURE.md` §3.2.3 and §6.3.

### Model routing

```json
{
  "providers": {
    "anthropic": { "api_type": "anthropic", "default_model": "claude-sonnet-4-6" },
    "local":     { "api_type": "openai_compatible", "base_url": "http://localhost:11434",
                   "default_model": "qwen3" }
  },
  "default_provider": "anthropic",
  "task_models": { "subagent": "local", "compact": "local" }
}
```

Task keys `main`, `subagent`, `compact`, `memory`, `web_search` and `web_fetch`
route each kind of call; a session can also choose its own provider. `WebSearch`
searches through the session's own model (the provider's built-in search) unless
`task_models.web_search` names another provider. `WebFetch` returns the page as
plain text (truncated at 50,000 characters); only when `task_models.web_fetch`
names a provider does that provider's model read the page and answer the call's
`prompt` instead. It never falls back to the session's own model.

### Context, telemetry, recording

- **Compaction**: an LLM summary first, then cheaper fallbacks (snip,
  micro-compact, collapse), aimed at what the threshold leaves beside the
  system prompt and tool definitions. Opt-in in the `compaction` settings: a
  predictive pass with a circuit breaker (`reactive`), and hourly clearing that
  emits cache edits so the prompt cache is not thrown away
  (`cached_microcompact`). A tool result over 30,000 characters (except those of
  `Read`, `WebFetch` and `TaskOutput`, which page themselves) is saved whole to the
  session's scratchpad; the model gets a head and tail preview with the path and
  reads on with `Read` `offset`. The limit is
  `compaction.tool_result_inline_chars` (`0` turns it off).
- **Telemetry**: 22 structured event types (`EventPayload` (22)) with
  OpenTelemetry export, four token counts per call, and the calls no turn asked
  for — compaction, memory, hooks, search — counted too.
- **Recorder**: wrap any `Model` and a session becomes a self-contained
  directory; replay is byte-exact and needs no network. A recording is not
  redacted — it holds the prompts, tool arguments and results as sent — so keep
  it the way you keep the transcript.

---

## Running `agixd`

| profile | command | on panic |
|---|---|---|
| `release` (the default) | `cargo build --release -p daemon --bin agixd` | aborts the process; about 11 MB |
| `release-fast` | `cargo build --profile release-fast -p daemon --bin agixd` | unwinds; a panic inside a task ends that task; twice the size, a little faster |

Commands the agent runs live in their own process groups and outlive a crashed
daemon, so the daemon cleans up twice: its panic hook stops them before
aborting, and the next start of the same instance stops what a killed one
(`SIGKILL`, OOM) left behind. Crash records go to `~/.agix/daemon/crashes/`.
The daemon does not restart itself — run it under launchd or systemd with
restart on failure; under systemd use `KillMode=process`, so the daemon, not
systemd, decides which background commands to stop.

Configuration is layered, lowest priority first: built-in defaults →
`~/.agix/settings.json` → `~/.agix/scenes/<scene>/settings.json` →
`<project>/.agix/settings.json`, each with an optional `settings.local.json`;
`permission_rules` from every tier apply side by side. The full surface is the
generated [JSON Schema](docs/schemas/settings.schema.json). Values are taken
literally: `$VAR` / `${VAR}` / `${VAR:-default}` are expanded only inside an
MCP server's `command`, `args`, `env`, `url` and `headers`.

A few switches whose effect the rest of this README does not mention:

| Setting | Default | Effect |
|---|---|---|
| `model.fallback_model` | none | the model a call is retried on when the main one stays overloaded (529) |
| `model.max_output_tokens` | by model name | the most `max_tokens` a reply cut off at its limit is retried with |
| `memory_enabled` | `true` | file-based memory: the memory prompt, the index in each turn's reminder, recall |
| `telemetry_enabled` | `true` | one telemetry JSONL file per session under the state directory's `telemetry/` |
| `telemetry_url` | none | OTLP endpoint (else `OTEL_EXPORTER_OTLP_ENDPOINT`); only in a build with the `otel` feature |
| `instruction_file` | none | one more instruction file, read when the session is built and given to the model like `AGENTS.md` |
| `hooks_config` | none | lifecycle hooks: event name → list of hooks ([docs/extension_points.md](docs/extension_points.md)) |
| `disable_skill_shell_execution` | `false` | a skill body's `` !`cmd` `` is replaced by a notice instead of run |
| `team.enabled` | `false` | gives `TeamCreate` / `TeamList` / `TeamDelete` to sessions whose scene supports teams (`code`, `harnix`) |
| `team.member_idle_timeout_secs` | `1800` | a team member idle this long is stopped |
| `sandbox.network_mode` | `unrestricted` | outbound network of sandboxed commands: `unrestricted`, `deny_all`, `allowlist` (`sandbox.allowed_domains`; no backend filters by domain, so a sandboxed command reaches only localhost on macOS when listed, and nothing on Linux), or `proxy` — to filter by domain, put a proxy in front |
| `sandbox.egress_proxy` | none | under `proxy`, the only address a sandboxed command may connect to (`"127.0.0.1:3128"`, or `{"http", "socks", "no_proxy"}`), named to it in `HTTP_PROXY` / `HTTPS_PROXY` / `ALL_PROXY` / `NO_PROXY`; macOS holds a proxy off this machine only to its port, Linux needs `socat` and cuts the network without it |

| Variable | Purpose |
|---|---|
| `ANTHROPIC_AUTH_TOKEN` / `ANTHROPIC_API_KEY` | model credentials (or the host supplies its own credential source) |
| `ANTHROPIC_BASE_URL` | a compatible endpoint |
| `AGIX_DAEMON_TOKEN` | TCP / WebSocket handshake token |
| `AGIX_CONFIG_HOME` | global root, default `~/.agix` |

---

## Build and test

Rust 1.88+. The test suite needs no network and no key.

```sh
cargo build --workspace
cargo test  --workspace                                      # everything offline
cargo test  -p blackbox --features real-model --test real_model  # the live tier, against a real model
```

From unit tests to the assembled product: crate tests, a behavior net that
runs a real `Agent` against written-down model answers, `daemon/tests` over a
real socket, architecture guards over the workspace's structure, and
`tests/blackbox` — the product driven over JSON-RPC, once against a scripted
model server and once (the `live_*` cases) against a real model behind a
recording proxy. How each layer works and how to run it:
[`tests/README.md`](tests/README.md).

---

## Documentation

| Document | For |
|---|---|
| [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) | how the engine is put together — layers, modules, the turn, sessions and scenes, history, execution, concurrency, disk layout |
| [docs/extension_points.md](docs/extension_points.md) | every seam: what you can replace, contribute to or intercept, what it costs, who may |
| [docs/hooks.md](docs/hooks.md) | the hook protocol: configuration, the four backends, exit codes, timeouts, payloads, answers, and when each event fires |
| [docs/telemetry.md](docs/telemetry.md) | the telemetry files, envelope and redaction, and every event type with its fields |
| [docs/daemon_rpc_protocol.md](docs/daemon_rpc_protocol.md) | the JSON-RPC reference — methods, events, error codes, auth |
| [docs/plugin_development.md](docs/plugin_development.md) | writing Rust and QuickJS plugins, with the examples in `examples/plugins/` |
| [docs/extending_quickjs.md](docs/extending_quickjs.md) | writing script extensions |
| [docs/plugin_packages.md](docs/plugin_packages.md) | the plugin package format, install and trust |
| [docs/ptc.md](docs/ptc.md) | `RunCode`: what a model's program can call, its permissions, limits and prompt |
| [docs/mcp_auth.md](docs/mcp_auth.md) | OAuth for remote MCP servers: configuration, sign-in flows, RPCs and events, credential storage, security |
| [docs/schemas/settings.schema.json](docs/schemas/settings.schema.json) | the generated schema of `settings.json` |
| [tests/README.md](tests/README.md) | the test layers and the live tier |

## Design principles

1. **Library-first.** The daemon is a reference host, not the product.
2. **The engine owns mechanism; you own policy.** Every decision is behind a trait.
3. **Extensions cannot break the engine.** A failure costs its own contribution.
4. **Safe by default.** You opt into less safety, never into more by accident.
5. **Nothing is claimed that is not checked** — including by these documents.

## License

Apache-2.0
