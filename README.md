# Agix

[![CI](https://github.com/harnixai/agix/actions/workflows/ci.yml/badge.svg)](https://github.com/harnixai/agix/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/harnixai/agix?display_name=tag&sort=semver)](https://github.com/harnixai/agix/releases)
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
   with `app.worker_threads` (`--worker-threads`) — one is enough on a small
   board — and leave out whole modules at build time with the build tiers.
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
   model, that came to **39% less wall time, 63% fewer prompt tokens, a fifth
   of the CPU and a thirteenth of the memory, with every run correct**
   ([measured](#measured)).
5. **Your product stays in charge.** Every session, task, sub-agent, goal and
   prompt is visible, stoppable and editable over one JSON-RPC protocol.
   47 extension points let you replace or intercept any subsystem from Rust
   or a QuickJS script without forking. Every session is an
   append-only log you can resume, fork, export or replay byte for byte
   with no network.

---

## Measured

Six engineering tasks, Agix 0.3.15 against DeepSeek Harness (DSH)
0.2.2-rc.1 on the same model. Medians of three rounds; all 36 runs correct.

| Task | Shape | Prompt tokens (Agix / DSH) | Wall s | CPU s | Peak memory MB |
|---|---|---|---|---|---|
| `stats` | write a small program from a precise spec | 29.3K / 44.5K | 13 / 28 | 0.56 / 3.55 | 18 / 278 |
| `inventory` | fix three bugs across a package, then add a CLI | 75.0K / 123K | 27 / 50 | 0.81 / 3.53 | 21 / 283 |
| `biglog` | answer questions about a 1.4 MB access log | 50.1K / 33.0K | 13 / 9 | 0.66 / 1.99 | 18 / 281 |
| `shop` | two turns on an unfamiliar project: a bug report, then a feature | 125K / 127K | 21 / 27 | 0.71 / 4.34 | 21 / 283 |
| `audit` | fan a 24-module review out to four sub-agents, fix and list what is broken | 369K / 1.74M | 70 / 141 | 3.05 / 16.95 | 22 / 303 |
| `migrate` | move 58 calls in 30 modules off a deprecated helper | 153K / 128K | 28 / 28 | 1.00 / 3.01 | 25 / 282 |
| **total** | | **802K / 2.19M** | **172 / 282** | **6.8 / 33.4** | |

- **Resources.** These are the steadiest figures: the rounds barely differ.
  The Agix daemon never passed 25 MB. One daemon serves every session,
  where DSH runs a node process of about 280 MB per session.
- **Tokens.** Most of the token saving is on `audit`, the sub-agent
  workflow. On `biglog` and `migrate` Agix spent more: it took one or two
  extra model calls to cross-check its own answer.
- **Variance.** Prompt tokens follow the number of model calls, so a single
  run can swing two- or threefold on either side; compare medians, not runs.

How it was run:

- **Configuration.** Agix ran a release `agixd` with the `harnix` scene,
  unattended under the default permission policy. DSH ran `--profile
  headless`. Both used `deepseek-flash`, thinking on, effort high. The two
  never ran at the same time.
- **Correctness.** Each result is checked by running it on inputs the model
  never saw.
- **Tokens.** These are what the provider reported, read off a recording
  proxy in front of each harness, sub-agents included.
- **CPU and peak memory.** These are the kernel's count when the harness
  process is reaped, children included. For Agix that is the daemon's whole
  life, start-up included.
- **Wall time.** DSH's includes its process start each turn; the Agix daemon
  is already up.
- **Machine.** Measured 2026-11-15 on an M2 Max (macOS VM, 8 cores, 16 GB).

DSH is not part of this repository. How to install it and run the comparison
yourself, and what each task and figure means:
[`docs/benchmark/compare_dsh.md`](docs/benchmark/compare_dsh.md). The full
record of this run — versions, environment and every round:
[`docs/benchmark/2026-11-15-agix-0.3.15-vs-dsh-0.2.2-rc.1.md`](docs/benchmark/2026-11-15-agix-0.3.15-vs-dsh-0.2.2-rc.1.md).

---

## Features

**Deployment**
- `agixd`: a daemon speaking JSON-RPC 2.0 (NDJSON) over a Unix socket, TCP
  or WebSocket, with token authentication off the local socket.
- Or embed the engine: one `Agent` from the builder, the daemon's whole
  session pool in your own process, or `agix::embed` — `agixd`'s arguments
  in, protocol connections out.
- `release` builds an 11 MB binary; `release-fast` trades twice the size for
  a little speed and unwinding on panic.
- Build tiers (`minimal`, `coding`, `full`) and per-module features: MCP,
  sub-agents, skills, web tools, structured output, `RunCode`, WebSocket,
  scripts, plugins, the sandbox, OpenTelemetry.
- `app.worker_threads` (`--worker-threads` / `AGIX_WORKER_THREADS`); session cap
  with least-recently-used eviction and idle timeout (`app.session_cap`,
  `app.session_idle_timeout_secs`); several scenes per process.
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
- `RunExt`: runs a command an installed plugin brings, as one command line
  (`forge spec list`); the plugin's JavaScript does the mechanical work.

**Multi-agent**
- The `Agent` tool with typed agents: foreground or background, with their
  own models and tools, and results validated against a JSON Schema.
- Teams with a mailbox and shared memory.
- Sub-agents run in sidechain sessions linked to their parent.
- Concurrency and depth limits, and permissions, hooks and budgets that
  follow delegation.

**Tools** — 46 built in: files, shell, web, tasks, planning, goals,
scheduling, git worktrees, skills, teams, MCP, questions, notifications,
monitors and structured output ([table below](#tools--46-built-in)).

**Context**
- An LLM summary first, then cheaper fallbacks, behind a predictive
  trigger, a circuit breaker and a configurable threshold.
- Session memory with recall.
- A skill catalog.
- Token budgets per turn, goal and session — sub-agents, team members and
  calls on the side included, judged before each call — and per-provider
  daily, weekly and monthly quotas.
- Settings in layers (global, scene, project, `agixd --set`, a session's own
  `options.settings`, the managed file) under one key table, with
  `config.explain` to show why a key has the value it has
  ([docs/settings.md](docs/settings.md)).

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
  where data goes (providers, endpoints, instruction files) wait for trust
  (granted in the global tier or over RPC; untrusted by default,
  [ARCHITECTURE.md](docs/ARCHITECTURE.md) §6.6).

**Host control**
- Ninety-two RPC methods: create, resume, fork, rebuild, rewind, export and import sessions;
  read history, the exact context of the next model call and a session's model-call
  recording; search sessions by content; answer
  permission and question prompts.
- Start, message, stop and follow tasks, reading their output by byte offset.
- Switch the model, prompt or permission mode; manage scenes, MCP servers, plugins,
  schedules and which projects are trusted.
- Provide tools and hooks from the host's own connection to every session
  (`daemon.setHostTools`, `daemon.setHooks`), report progress on host tools,
  and give a session skill directories of its own (`options.skill_dirs`).

**Extensibility**
- 47 published extension points: 31 contracts, 10 interceptions and 6
  registrations.
- Rust plugins compiled into the host, and QuickJS scripts, installable as
  packages with install-time disclosure.
- Hooks at twenty-nine moments, with command, prompt, HTTP and agent backends,
  and hooks a host connection registers over RPC
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
  write (or `--managed-settings`): they may set any key and sit above every
  other layer, their deny and ask rules hold even under `bypassPermissions`,
  and they can switch that mode off; an unreadable file
  refuses every tool call ([ARCHITECTURE.md](docs/ARCHITECTURE.md) §6.1).

**Records**
- An append-only JSONL history that can be resumed, forked or rebuilt after
  a crash.
- 22 telemetry event types, with OpenTelemetry export (metrics and a span tree)
  ([`docs/telemetry.md`](docs/telemetry.md)).
- A recorder for byte-exact offline replay; a session's recording is kept
  with it and read over RPC (`session.calls`).
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
                settings · memory · prompt-files · session-state (K1)
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

The engine publishes a catalog of its seams, generated from the code: **47
extension points** — 31 *contracts* (replace a subsystem), 10 *interceptions* (sit
in the path of something in flight), 6 *registrations* (contribute to a
collection). From Rust, implement the trait and hand it to the builder. From
outside a Rust build:

| Route | Reaches | Cost |
|---|---|---|
| **Scripts** (QuickJS, default) | nine points: four that write the prompt, two around a tool call, two around a model call, memory recall | a `.js` file and one line of config; microseconds per call |
| **Plugin packages** | MCP servers, scripts, a declarative scene, agent types, skills, commands the model runs through `RunExt` | a zip with a `plugin.toml`; everything it will show the model is disclosed at install |
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
| kernel | `settings` · `memory` · `prompt-files` · `session-state` | settings tiers and project trust, file-based memory, the markdown files read into the prompt (`SKILL.md`, `AGENTS.md`, rules), one session's mutable state |
| kernel | `ptc` | `RunCode`: the model's programs, run in an embedded QuickJS ([`docs/ptc.md`](docs/ptc.md)) |
| library | `js-lib` | the `agix` global every embedded QuickJS gets: YAML, glob, paths, JSON Schema ([`docs/agix_js.md`](docs/agix_js.md)) |
| module | `agents` · `team` | the `Agent` tool, agent types, teams |
| module | `mcp` | MCP client and tool adapters, MCP OAuth authorization |
| module | `skills` | skill catalog and the `Skill` tool |
| module | `script-host` | the QuickJS carrier |
| module | `packages` | plugin packages: manifest, install, cache, disclosure |
| assembly | `assembly` | session pool, configuration, provider routing, the JSON-RPC server |
| shell | `agix` | the embedding facade: re-exports by theme, no logic of its own |
| shell | `daemon` | the `agixd` binary |

The names above are crate names, the ones code `use`s. As packages — for
`cargo -p` and on crates.io — they are `agix-<crate>` (`agix-base`,
`agix-runtime`, …), except the facade, `agix`, and the daemon, `agixd`. Those
two are what you depend on or install; the `agix-*` crates are their
implementation.

26 crates under `crates/`, plus `daemon/`, the seven test members and the
example plugin in `examples/plugins/rust` — 35 workspace members in total.

---

## Quick start

### Run the daemon

```sh
cargo install agixd
export ANTHROPIC_AUTH_TOKEN=sk-...        # or ANTHROPIC_API_KEY; ANTHROPIC_BASE_URL for a compatible endpoint
agixd                                     # listens on ~/.agix/scenes/code/daemon.sock
```

`cargo install` builds with Cargo's default release profile, not this
workspace's size-tuned one ([Running `agixd`](#running-agixd)), so the binary
is larger. Or from a checkout:

```sh
git clone https://github.com/harnixai/agix && cd agix
export ANTHROPIC_AUTH_TOKEN=sk-...        # or ANTHROPIC_API_KEY; ANTHROPIC_BASE_URL for a compatible endpoint
cargo run --release -p agixd --bin agixd # listens on ~/.agix/scenes/code/daemon.sock
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
(`session.subscribe`) and drives it from another. Ninety-seven methods are
documented in [`docs/daemon_rpc_protocol.md`](docs/daemon_rpc_protocol.md) —
sessions, history and context, tasks and sub-agents, scenes, configuration,
project trust, MCP, plugins, scheduling — and every documented response shape is compared
against a real daemon's answer by a test.

Useful flags: `--scene` / `--scenes` (which scenes to serve), `--listen` /
`--listen-ws` with `--token` (TCP / WebSocket, authenticated), `--unix-socket-auth` (the unix socket authenticates too),
`--mcp-policy`, `--managed-settings`, and `--set KEY=VALUE`
for any setting over every settings file (`--set permissions.mode=plan`,
`--set 'files.read_dirs=["/data"]'`, `--set turn.max_model_calls=50`; the value
is read as JSON, else as a string; repeatable). Shorthands for `--set`:
`--max-tokens N` (`model.max_tokens`), `--permission-prompt-timeout N`
(`permissions.prompt_timeout_secs`), `--session-cap N` (`app.session_cap`),
`--session-idle-timeout N` (`app.session_idle_timeout_secs`) and
`--worker-threads N` (`app.worker_threads`; `AGIX_WORKER_THREADS` comes before
every flag, so a flag wins). Like any `app` key, these three can be written in
the global `settings.json` instead of a service's command line. A key no
setting reads, an old key or a value that does not fit stops the daemon at
startup, naming the `--set`.
Each running daemon writes a discovery file
under `~/.agix/daemon/instances.d/`, so a client finds it by scene instead of
by path.

### Embed the engine

```toml
[dependencies]
agix = "0.4"
# or a release tag from git:
# agix = { git = "https://github.com/harnixai/Agix", tag = "Agix-v0.4.4-2026112501" }
```

crates.io carries each release as its plain `x.y.z`; the git tag
`Agix-v<x.y.z>-<YYYYMMDDNN>` names the same code
([`docs/agix-sdk.md`](docs/agix-sdk.md) §9).

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
ANTHROPIC_API_KEY=... cargo run --release -p agix-runtime --example minimal -- "list the files here"
```

As a session pool — the daemon's engine in your own process, with the same
settings tiers, routing, history layout and modules, and your own scenes:

```rust
let pool = agix::pool::assemble(
    &agix::pool::load_daemon_config("claude-sonnet-4-6", None, None, "code", &paths),
    Arc::new(agix::scenes::code::spec(agix::scenes::code::Interaction::Interactive)),
    agix::pool::Assembly { scenes: vec![my_scene], ..Default::default() },
)
.await?;
```

`agixd` is this call plus a command line and its listeners; a test fails the
build if anything in the workspace assembles a pool a second way.

As a protocol endpoint — `agix::embed`, which `agixd` itself starts from:
`EmbedConfig` takes `agixd`'s arguments as fields, `Embedded::start` brings the
engine up, and `Embedded::serve_stream` serves the JSON-RPC protocol on any byte
stream in your process (`serve_unix` on a socket), with no other Agix type in
sight. It is the one Rust surface 0.x keeps stable ([`docs/agix-sdk.md`](docs/agix-sdk.md) §10). The `agix` crate is the one an embedder depends on; [`docs/agix-sdk.md`](docs/agix-sdk.md) is the guide.

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

### Tools — 46 built in

| Category | Tools |
|---|---|
| **Filesystem** | `Read` `Write` `Edit` `Glob` `Grep` |
| **Shell** | `Bash` (sandboxed, path-safe, timeout-controlled) |
| **Web** | `WebFetch` `WebSearch` |
| **Task** (running sub-agents, shell commands, monitors) | `TaskOutput` `TaskWait` `TaskStop` `TaskMessage` |
| **Planning** | `EnterPlanMode` `ExitPlanMode` `TodoWrite` |
| **Goals** | `GoalCreate` `GoalGet` `GoalUpdate` (a goal keeps the session working across automatic rounds, until it is done or blocked, or its round limit or optional token budget run out) |
| **Scheduling** | `CronCreate` `CronDelete` `CronList` |
| **Workspace** | `EnterWorktree` `ExitWorktree` (isolated git worktree per session) |
| **Collaboration** | `Skill` `Agent` `TeamCreate` `TeamList` `TeamDelete` |
| **Team members** (given only to a team's members) | `SendMessage` `ReadMail` `ListPeers` `TeamMemorySave` `TeamMemoryRead` |
| **Protocol** | `MCP` `ListMcpResources` `ReadMcpResource`, plus one `mcp__<server>__<tool>` per discovered MCP tool |
| **Interaction** | `AskUserQuestion` `PushNotification` `Monitor` `StructuredOutput` |
| **Programs** | `RunCode` (a JavaScript program that calls the session's tools; only its return value comes back), `RunExt` (a command line an installed plugin answers; present when one declares a command) |
| **Editor / Meta / Diagnostics** | `NotebookEdit` · `ToolSearch` · `Ping` `Sleep` |

`tools::register_builtin_tools` installs the 21 self-contained ones; the rest
need per-session state and are registered by the builder or by a module.
Deferred tools are advertised by name and fetched with `ToolSearch` when needed.
The `code` scene leaves out `NotebookEdit`, the worktree tools, `Ping`, `Sleep`
and `PushNotification`; `StructuredOutput` is there only
when the host sets an output schema. A scene is interactive (someone answers
prompts) or unattended: `harnix` and `mini` are unattended, `code` is created
either way, and a session can override its scene's choice (`options.attended`). An unattended session has no `AskUserQuestion` or plan mode, refuses
what would need approval instead of asking, and runs a `Bash` command no allow
rule covers only when the OS sandbox fully holds it and the sandbox network is
restricted (`sandbox.network.mode`) — see `docs/ARCHITECTURE.md` §3.2.3 and §6.3.

### Model routing

```json
{
  "providers": [
    { "id": "ds", "vendor": "deepseek", "api": "openai_chat",
      "base_url": "https://api.deepseek.com", "api_key_env": "DEEPSEEK_API_KEY",
      "models": ["deepseek-v4-pro", "deepseek-v4-flash"],
      "tiers": { "strong": "deepseek-v4-pro", "balanced": "deepseek-v4-flash" },
      "quota": { "day": 20000000, "month": 300000000 } },
    { "id": "claude", "vendor": "anthropic", "api": "anthropic",
      "api_key_env": "ANTHROPIC_API_KEY",
      "tiers": { "strong": "claude-opus-4-6", "balanced": "claude-sonnet-4-6",
                 "fast": "claude-haiku-4-5" } }
  ],
  "model": { "order": ["ds", "claude"], "tasks": { "subagent": "balanced" } }
}
```

A provider has an `id`, a `vendor` (`anthropic`, `openai`, `deepseek`, `qwen`,
`vllm`, `sglang` or `generic`; it decides how thinking is switched, whether
reasoning is sent back and how effort is spelled), an `api` (`anthropic`,
`openai_chat` or `openai_responses`), a `base_url`, the environment variable
holding its key (`api_key_env`; there is no key in settings, and a host
embedding the engine can supply keys itself), its `models`, its `tiers` and an
optional `quota`.

Each kind of call asks for a tier — `strong`, `balanced` or `fast` — set per
task in `model.tasks` (`main`, `subagent`, `compact`, `memory`, `title`,
`web_search`, `web_fetch`; the defaults are `strong`, `balanced` and `fast`, and
`web_fetch` is unset), and each provider says which of its models serves a tier:
`strong` is required, `balanced` defaults to `strong`'s model, `fast` to
`balanced`'s.

Every call starts at the first provider of `model.order` and goes to the next
when one is out of quota, has no credential, refuses its credential, cannot be
reached, or keeps answering 5xx / 529 / 429 until its retries are spent, and
on a 402; a 400 is the request's own fault and is not retried elsewhere. A
provider that could not be reached is left alone for a short while. When every
provider in the order is unavailable the turn fails with `providers_unavailable`
and the reason for each. Switching drops the previous provider's private
reasoning content, and the session is told with a `provider_switched` event
(and a telemetry event of the same name).

A `quota` is a token limit per `day`, `week` or `month`, any of them. Tokens are
counted as input that missed the cache plus cache writes plus output; cache
reads are not counted. Periods follow this machine's local time, the week
starting on Monday and the month on the 1st. Usage is kept per provider id under
the state directory's `quota/`, shared by every instance on the machine; the call
that crosses a limit finishes, and the provider is skipped from the next one on.
`config.getProvider` reports usage against each limit.

With no `providers`, the engine supplies `anthropic`, keyed from
`ANTHROPIC_AUTH_TOKEN` / `ANTHROPIC_API_KEY`, every tier `--model`. That
built-in provider exists only while no `providers` are defined, and its model is
`--model`; to give it a quota or tiers, define it in `providers` (a host's
in-memory edit to the built-in one is not kept across a reload). A session can
put a provider of its own first, and ask for a tier or a model id.
`WebFetch` returns the page as plain text in windows of up to 50,000 characters,
read on with `offset` / `limit` (each continuation fetches the page again); only
when `model.tasks.web_fetch` names a tier does a model read the page and answer
the call's `prompt` instead.

### Context, telemetry, recording

- **Compaction**: an LLM summary first, then cheaper fallbacks (snip,
  micro-compact, collapse), aimed at what the threshold leaves beside the
  system prompt and tool definitions. Opt-in in the `compaction` settings: a
  predictive pass with a circuit breaker (`reactive`), and hourly clearing that
  emits cache edits so the prompt cache is not thrown away
  (`clear_on_cache_expiry`). A tool result over 30,000 characters (except those of
  `Read`, `WebFetch` and `TaskOutput`, which page themselves) is saved whole to the
  project's `.agix/state/tool-results/`; the model gets a head and tail preview with the path and
  reads on with `Read` `offset`. The limit is
  `compaction.inline_result_chars` (`0` turns it off).
- **Telemetry**: 23 structured event types (`EventPayload` (23)) with
  OpenTelemetry export, four token counts per call, and the calls no turn asked
  for — compaction, memory, hooks, search — counted too.
- **Recorder**: wrap any `Model` and a session becomes a self-contained
  directory; replay is byte-exact and needs no network. A recording is not
  redacted — it holds the prompts, tool arguments and results as sent — so keep
  it the way you keep the transcript. A daemon session that records keeps its
  recorder in its log, so it records on after a restart, and `session.calls` /
  `session.call` read the recording over RPC.

---

## Running `agixd`

| profile | command | on panic |
|---|---|---|
| `release` (the default) | `cargo build --release -p agixd --bin agixd` | aborts the process; about 11 MB |
| `release-fast` | `cargo build --profile release-fast -p agixd --bin agixd` | unwinds; a panic inside a task ends that task; twice the size, a little faster |

Commands the agent runs live in their own process groups and outlive a crashed
daemon, so the daemon cleans up twice: its panic hook stops them before
aborting, and the next start of the same instance stops what a killed one
(`SIGKILL`, OOM) left behind. Crash records go to `~/.agix/daemon/crashes/`.
The daemon does not restart itself — run it under launchd or systemd with
restart on failure; under systemd use `KillMode=process`, so the daemon, not
systemd, decides which background commands to stop.

Configuration is layered, lowest priority first: built-in defaults →
`~/.agix/settings.json` → `~/.agix/scenes/<scene>/settings.json` →
`<project>/.agix/settings.json`, each with an optional `settings.local.json`
that outranks the file beside it → the `cli` layer (`agixd --set`, or an
embedder's `EmbedConfig.settings`), over every project and scene. **Every key with its default, scope, project-trust rule and when a change applies: [docs/settings.md](docs/settings.md)** (Chinese). The generated
[JSON Schema](docs/schemas/settings.schema.json) is the machine-readable form. Values are taken literally:
`$VAR` / `${VAR}` / `${VAR:-default}` are expanded only inside an MCP server's
`command`, `args`, `env`, `url` and `headers`. A key the engine no longer reads
is ignored, and the load warning names the key that replaced it.

How strictly a bad setting is treated depends on who wrote it. Whoever is writing
and present gets an error; a file is warned about; the security file fails
closed. An unknown or renamed key or a value that does not fit, in a settings
file, is warned about and ignored (`daemon.doctor` and `config.explain` show it);
in `agixd --set` or `EmbedConfig.settings` it fails startup; in `options.settings`
or any other RPC parameter it fails the request with `INVALID_PARAMS`; in the
managed file it applies nothing and refuses every tool call.

How layers combine:

- Objects merge key by key; a scalar or a list set in a higher layer replaces
  the lower one's (a project's `model.order` replaces the global one).
- `providers` holds definitions, so layers add to it; an id already defined in
  a layer read earlier wins and the later one is ignored with a warning.
- `permissions` rule lists (`allow`, `allow_exact`, `ask`, `deny`) are not
  merged: every layer's rules apply side by side, and a deny wins over any
  allow.
- `hooks` merge per event: a layer that lists an event replaces the lower
  layers' list for it. `mcp.servers` merge by name and field.
- `app` is read from the global layer (and the `cli` one) only; a scene or
  project that sets it is ignored with a warning.
- The `cli` layer merges like the global one; it is not gated by project trust,
  and `config.reload` reads the files again beneath it. `config.get
  {tier:"cli"}` and `daemon.doctor` report it as `cli`.
- A deployment's managed file may set any key (plus
  `permissions.disable_bypass`); what it sets outranks every layer, the `cli`
  one, a host's in-memory changes and a session's own `options.settings`. Its
  rule lists stand beside every layer's, and its deny and ask rules hold even
  in `bypassPermissions`. A file that does not read sets nothing and refuses
  every tool call.
- A project the user has not trusted can only tighten (see Workspace trust):
  `providers`, `hooks`, `mcp.servers` entries, `prompt.instruction_files`,
  `prompt.override`, `sandbox.network.proxy`, `telemetry.otlp_endpoint` and
  `recorder` wait for trust, and its `model.order` may name only providers
  defined above it.

The table of every place a value can come from, in priority order, is in the [settings reference](docs/settings.md#设置从哪来) (Chinese).

To find out why a key has its value, `config.explain {key}` (or a block, such as `turn`; add
`session_id` for a session's) lists each of these layers with what it set and what became of it there:
applied, overridden, withheld from an untrusted project, dropped as global-only, ignored, overruled by
the managed file, or clamped for the session.

`session.create`'s `model` and `provider` are not settings: they choose the model the session's
conversation runs on. Its effort, thinking and prompt are: `options.settings.model.effort`,
`model.thinking`, `prompt.append` / `prompt.override`, for the session and its sub-agents alike, and
`session.setModel` / `session.setPrompt` change that same layer.

The blocks:

| Block | What it holds |
|---|---|
| `providers`, `model` | the model providers, the tier each kind of call asks for, the fallback order ([Model routing](#model-routing)); `model.effort`, `model.thinking` (`auto`, `on`, `off`), `model.max_tokens` |
| `permissions` | `mode`, the rule lists as strings (`allow`, `ask`, `deny`; `allow_exact` compares its content as written and is where "allow every time" records), `prompt_timeout_secs` (an unanswered prompt is denied), `client_may_loosen` (whether an RPC client may ask for a looser mode than `mode`) |
| `files` | `extra_dirs` (read and write like the project), `read_dirs` (read without asking; `"/"` reads anywhere), `allow_write`, `deny_write` |
| `sandbox` | `enabled`, `require` (refuse to run a command when the platform cannot enforce the sandbox), `allow_read`, `deny_read` (added to the built-in credential list), `network`, `env`, `cache_dirs`, `reap_on_close`, `unattended_needs_network_policy` (on by default: an unattended command with no allow rule also needs `network.mode` other than `unrestricted`) |
| `web` | `allowed_domains` for `WebFetch` and `Ping`, and the list `sandbox.network.mode: "allowlist"` reads |
| `turn`, `session`, `goal`, `schedule`, `agents`, `team` | `turn.max_model_calls`, `turn.token_budget`, `turn.parallel_tools`; `session.token_budget`; `goal.max_rounds`; `schedule.max_model_jobs`, `schedule.min_model_interval_minutes`; `agents.max_depth`, `agents.max_concurrent`; `team.enabled`, `team.stage_concurrency`, `team.max_members`, `team.member_idle_secs` |
| `compaction` | `threshold_tokens`, `keep_recent`, `reactive`, `clear_on_cache_expiry`, `inline_result_chars` |
| `prompt` | `append`, `override`, `instruction_files`, `language`, `path_style` (`absolute`, `relative`) |
| `hooks`, `scripts`, `plugins`, `mcp`, `skills` | lifecycle hooks ([docs/hooks.md](docs/hooks.md)), script bindings, the plugin switches (`plugins.enable_on_install` is off by default: the host calls `plugin.enable` after reading the install's disclosure), `mcp.servers.<name>` (an entry holding only `enabled` switches a server declared elsewhere), `skills.allow_shell` |
| `memory`, `telemetry`, `checkpoints`, `recorder` | `memory.enabled`; `telemetry.enabled`, `telemetry.otlp_endpoint`; file checkpoints; LLM recording |
| `app` | the global file layer, the `cli` layer and the managed file only: `trusted_projects`, `trust_all_projects`, `history`, `system_proxy`, `worktree_dir`, `session_cap`, `session_idle_timeout_secs`, `worker_threads` |

A few switches whose effect the rest of this README does not mention:

| Setting | Default | Effect |
|---|---|---|
| `providers[].models[].max_output` | by model name | the most `max_tokens` a call to the model is sent with, and a reply cut off at its limit is retried with |
| `memory.enabled` | `true` | file-based memory: the memory prompt, the index in each turn's reminder, recall |
| `telemetry.enabled` | `true` | one telemetry JSONL file per session under the project's `.agix/state/telemetry/` (the state directory's `telemetry/` without a project) |
| `telemetry.otlp_endpoint` | none | OTLP endpoint (else `OTEL_EXPORTER_OTLP_ENDPOINT`); only in a build with the `otel` feature |
| `prompt.instruction_files` | `[]` | more instruction files, read in order when the session is built and given to the model like `AGENTS.md` |
| `turn.token_budget` | none (unlimited) | the tokens one turn may spend — input not read from the cache, cache writes and output, as provider quotas count them — together with the sub-agents and team members it starts or hands work to and the calls made on the side (compaction, memory, `WebSearch`, `WebFetch` extraction, `prompt` and `agent` hooks); before each call the input it will be charged for (what is not expected from the provider's cache) is estimated and taken off what is left, the call asks for no more output than remains, and the turn ends with `budget_exceeded` when that cannot pay for a call. An estimate, not a reservation: sub-agents running at once can overshoot by one call each. A session can only lower it, with `session.create`'s `options.settings.turn` |
| `session.token_budget` | none (unlimited) | the tokens a session may spend from when it was created, counted as `turn.token_budget` is, over every turn — its sub-agents', team members', calls on the side, scheduled and goal turns included — and across restarts (the running total is in the session log; `session.get` reports it as `usage`). Each call is judged against it as against `turn.token_budget`, its estimated input first; once what is left cannot pay for a call the turn ends with `budget_exceeded`, and every later turn does too without calling the model |
| `schedule.max_model_jobs` | `20` | the most scheduled jobs the model's own `CronCreate` calls may hold in a session at once; another is refused with the reason in the tool result. Jobs a host creates with `schedule.create` are not counted or held |
| `schedule.min_model_interval_minutes` | `1` | the fewest minutes between two fires of a recurring job the model schedules (over the seven days it lives); `1` adds nothing to cron's own resolution. A one-shot job is not held to it; how many the model holds at once is `max_model_jobs`, and what their turns spend is the session's |
| `goal.max_rounds` | `10000` | the most automatic rounds a goal the model sets or edits (`GoalCreate`, `GoalUpdate`) may have; a larger `max_rounds` is lowered to it. A goal's own `token_budget` (set with `session.setGoal` or `GoalCreate`) blocks it once its rounds have spent it; `GoalUpdate` may lower that budget but not raise it, which is the host's (`session.updateGoal`) |
| `hooks` | none | lifecycle hooks: event name → list of hooks ([docs/extension_points.md](docs/extension_points.md)) |
| `skills.allow_shell` | `true` | `false` replaces a skill body's `` !`cmd` `` with a notice instead of running it |
| `team.enabled` | `false` | gives `TeamCreate` / `TeamList` / `TeamDelete` to sessions whose scene supports teams (`code`, `harnix`) |
| `team.member_idle_secs` | `1800` | a team member idle this long is stopped |
| `sandbox.enabled` | `true` | `false` runs commands without the OS sandbox |
| `sandbox.network.mode` | `unrestricted` | outbound network of sandboxed commands: `unrestricted`, `deny_all`, `allowlist` (`web.allowed_domains`; no backend filters by domain, so a sandboxed command reaches only localhost on macOS when listed, and nothing on Linux), or `proxy` — to filter by domain, put a proxy in front |
| `sandbox.network.proxy` | none | under `proxy`, the only address a sandboxed command may connect to (`"127.0.0.1:3128"`, or `{"http", "socks", "no_proxy"}`), named to it in `HTTP_PROXY` / `HTTPS_PROXY` / `ALL_PROXY` / `NO_PROXY`; macOS holds a proxy off this machine only to its port, Linux needs `socat` and cuts the network without it |
| `app.system_proxy` | `true` | the engine's own HTTP (model calls, `WebFetch` / `Ping`, MCP over HTTP, OAuth) follows `HTTP_PROXY` / `HTTPS_PROXY` / `ALL_PROXY` / `NO_PROXY`; `false` connects directly. Applied at daemon start; unrelated to the sandboxed commands' network above |
| `app.history` | `global` | where sessions are kept. `global`: under the state directory's `projects/` and `sessions/`. `project`: a trusted project's sessions go to its `.agix/state/history/<id>/` (a worktree's to its main checkout's) with only an index left under `sessions/<id>/`; untrusted projects stay global. History already kept either way is read where it is; history in a project is read only while the project is trusted. Applied at daemon start |
| `app.worktree_dir` | `.wkts` | the directory at the repository root that `EnterWorktree` and `Agent`'s `worktree` put git worktrees in (`<repo>/.wkts/<name>`), and the one line added to the repository's `.gitignore`; one path component starting with `.`, not starting with `.git`, and not a configuration directory or another tool's |
| `app.session_cap` | `32` | sessions kept in memory; one more takes the least recently active idle one out of memory, and its next use brings it back from disk. Applied at daemon start; `config.reload` does not change it |
| `app.session_idle_timeout_secs` | `3600` | seconds without activity before a session is taken out of memory. Applied at daemon start; `config.reload` does not change it |
| `app.worker_threads` | one per CPU core | `agixd`'s async worker threads, at least 1. Read once before anything starts; `config.reload` does not change it. An embedded engine runs on the host's runtime |

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
| [docs/settings.md](docs/settings.md) | the settings reference (Chinese): every `settings.json` key with its default, scope, project-trust rule and when a change applies; where values come from and how layers combine |
| [docs/upgrading-0.4.md](docs/upgrading-0.4.md) | upgrading an application from 0.3.x: the settings renames, `options.settings`, the protocol and budget changes, `EmbedConfig` |
| [docs/agix-sdk.md](docs/agix-sdk.md) | embedding the engine in your own Rust process: the facade crate, one pool, one turn, events, host tools, shutdown, and the `agix::embed` entry |
| [docs/extension_points.md](docs/extension_points.md) | every seam: what you can replace, contribute to or intercept, what it costs, who may |
| [docs/hooks.md](docs/hooks.md) | the hook protocol: configuration, the four backends, exit codes, timeouts, payloads, answers, and when each event fires |
| [docs/telemetry.md](docs/telemetry.md) | the telemetry files, envelope and redaction, and every event type with its fields |
| [docs/daemon_rpc_protocol.md](docs/daemon_rpc_protocol.md) | the JSON-RPC reference — methods, events, error codes, auth |
| [docs/plugin_development.md](docs/plugin_development.md) | writing Rust and QuickJS plugins, with the examples in `examples/plugins/` |
| [docs/extending_quickjs.md](docs/extending_quickjs.md) | writing script extensions |
| [docs/plugin_packages.md](docs/plugin_packages.md) | the plugin package format, install and trust |
| [docs/ptc.md](docs/ptc.md) | `RunCode`: what a model's program can call, its permissions, limits and prompt |
| [docs/runext.md](docs/runext.md) | `RunExt`: plugin commands — declaring them, the command line, permissions and `writes`, limits, slash form |
| [docs/agix_js.md](docs/agix_js.md) | the `agix` global in embedded scripts: every function, its limits and error codes |
| [docs/forge.md](docs/forge.md) | `forge`, the sample plugin: requirements, specs and a change flow as plugin commands |
| [docs/mcp_auth.md](docs/mcp_auth.md) | OAuth for remote MCP servers: configuration, sign-in flows, RPCs and events, credential storage, security |
| [docs/schemas/settings.schema.json](docs/schemas/settings.schema.json) | the generated schema of `settings.json` |
| [docs/benchmark/](docs/benchmark/compare_dsh.md) | the comparison against DeepSeek Harness: setup, the six tasks, what is measured and how, and the record of each run |
| [tests/README.md](tests/README.md) | the test layers and the live tier |

## Design principles

1. **Library-first.** The daemon is a reference host, not the product.
2. **The engine owns mechanism; you own policy.** Every decision is behind a trait.
3. **Extensions cannot break the engine.** A failure costs its own contribution.
4. **Safe by default.** You opt into less safety, never into more by accident.
5. **Nothing is claimed that is not checked** — including by these documents.

## License

Apache-2.0
