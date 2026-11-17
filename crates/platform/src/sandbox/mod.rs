//! The platform sandbox backends, macOS `sandbox-exec` and Linux `bubblewrap`: each
//! confines a command for a [`SandboxPolicy`] and reports what it delivered as `Enforcement`.
//! The policy follows the call's `ExecProfile` (`tools::bash::sandbox`): Session is the
//! session's; Unattended adds `deny_ipc_escapes`; Strict (`SandboxPolicy::strict`) cuts the
//! network, writes only `TMPDIR`, reads only its roots (`ReadScope::Only`), env `Minimal`.
//!
//! - **macOS**: `sandbox-exec -p <profile> <program> <args…>`. The profile is
//!   TinyScheme denying writes outside the allowed subtrees; exec is not
//!   restricted; under `ReadScope::Only` reads are denied but under each root.
//!   Network defaults to open; `DenyAll` / `Allowlist` both use
//!   `(deny network*)`. A profile names a remote host only as `*` or
//!   `localhost`, so the allowlist re-allows localhost when it names it and
//!   no domain at all; the domains it names are reported unmet through
//!   `Enforcement`, as on Linux. `Proxy` re-allows only the proxy's ports on
//!   `localhost` (which covers `127.0.0.1` and `::1`); a proxy off this
//!   machine can be held only to its port, on any host, and is reported
//!   unmet. Name resolution goes through `mDNSResponder`'s unix socket, which
//!   `(deny network*)` covers (measured on macOS 26).
//! - **Linux**: `bwrap --ro-bind / / --bind <writable> <writable> … <program>`
//!   (`ReadScope::Only`: a read-only bind per root). No bwrap: no constraint. `DenyAll` uses
//!   `--unshare-net`; bwrap has no domain-level filter, so `Allowlist`
//!   degrades to the same whole-network cut rather than silently opening up —
//!   failure goes toward the safer side, and says so through `Enforcement`.
//!   `Proxy` cuts the network too, then binds a unix socket into the
//!   namespace that this process forwards to the proxy (`egress::relay`),
//!   and a `socat` started inside listens on the namespace's
//!   `127.0.0.1:<proxy port>` and connects to it. Without `socat` in the
//!   system directories the network stays cut, reported unmet.
//!   Each command gets a PID namespace of its own, so the pids it sees (`$!`
//!   among them) are not the host's, and a later command does not see what an
//!   earlier one left running. What it left running outlives it, as on macOS.
//! - **Windows**: no backend.
//!
//! # What a confined command may write
//!
//! The working directory, the policy's `additional_writable` (the caller puts
//! the session's own `TMPDIR` there), `/dev`, and on macOS the user's
//! `DARWIN_USER_TEMP_DIR`, which `mktemp(1)` uses whatever `TMPDIR` says. On
//! Linux `/tmp` is a private tmpfs that goes away with the command. Every path
//! is resolved through its symlinks before a rule is written for it.
//!
//! Inside those, what the engine or git later acts on outside the sandbox is
//! read-only — where the policy names a `worktree_git` (the working
//! directory is a linked worktree), `.git` below means its git directory,
//! never one found by following the worktree's links: the
//! project's `.git`, `.agix` and `.claude` entries themselves,
//! `.git/{config,config.worktree,commondir}` and `.git/hooks/`, the hooks directory
//! `core.hooksPath` names and the files `.git/config` includes, the same in
//! the git directory of every submodule under `.git/modules/` (nested ones
//! included, and the directories leading down to them), the
//! instance's own project directory but its working entries — `scratchpad/`
//! and `memory/`, all the gate leaves out of configuration there
//! (`base::paths::ConfigRootAt::writable_entry`) — with its configuration
//! (`base::paths::ENGINE_CONFIG_*`: settings, MCP, hooks, commands, agents,
//! skills, plugins, rules, scenes, policy, scripts, output styles, and the
//! saved tool results the engine exports and forks) read-only even inside a
//! writable root the policy names there, all of a foreign configuration root
//! (`.claude`, and `.agix` under a renamed layout), and in either the
//! writable roots the policy names inside it stay writable, the project's skills (`.agents/skills/`,
//! `base::paths::PROJECT_ENGINE_CONFIG_DIRS`, and the `.agents` entry above
//! it), the control files of each linked worktree's entry under
//! `.git/worktrees/` (`base::paths::GIT_WORKTREE_CONTROL_FILES`), and the
//! `.git` links of the engine's worktrees (in the project's worktrees
//! directories, `.wkts/` unless the settings name another —
//! `base::paths::SensitivePaths::worktree_dirs_in`) — the last two for a
//! worktree made after the command started as well, so that what it leaves
//! running cannot rewrite those of a sub-agent's worktree made later: macOS
//! by pattern, with the worktrees directory and each worktree directly in it
//! held in place, Linux by holding `.git/worktrees/` (made, empty, where it
//! is absent) and the worktrees directory read-only with the worktrees
//! already in it opened again. A command therefore cannot add a worktree of
//! its own, nor move one aside to rewrite its link. Under
//! `only_additional_writable`, the engine's own git, which adds them, is held
//! only to the ones that exist.
//! A command in a linked worktree writes of its git directory only what
//! `additional_writable` names (`base::paths::WorktreeGit::writable`): the
//! object store but its `objects/info/`, its own entry under `worktrees/`
//! with an absent control file there shut too (on Linux by an empty file),
//! and its branch's ref, lock and reflog. On Linux the ref is opened as the
//! directory it is in — git renames `<ref>.lock` over it, which a bind of the
//! file refuses — with the other entries there read-only again, and the git
//! directory is bound read-only beneath, so that `/tmp` does not hide it.
//! Of the state root only `scratchpad/`, `memory/` and those of
//! `additional_writable` under `tmp/` (a session's own `TMPDIR`) stay
//! writable. A project that lives in the state root lifts that fence; the
//! state root is then guarded as the `.agix` it is — its configuration files
//! and directories are read-only, but for one the project itself is in —
//! and the settings and definitions of the root and every scene stay
//! read-only either way. On Linux the own project directory and each
//! foreign root is read-only as a whole and the own one's existing working
//! entries are opened again, so a new entry directly in either cannot be
//! created; `.agents/skills/` is read-only. A foreign root, `.agents` or
//! `.agents/skills` that is not there (or is an empty directory) is shut
//! under an empty read-only tmpfs, and the mount point bwrap makes for it is
//! removed again once no sandboxed command's namespace lives in the project
//! — not when bwrap exits, which can be before what the command left behind
//! (`linux::bwrap_stdio_outside`); the own directory's stays, as the
//! engine's own.
//!
//! A policy with `only_additional_writable` leaves out the working directory,
//! the state root's working entries and `DARWIN_USER_TEMP_DIR`: only
//! `additional_writable` and `/dev` are writable, the read-only entries above
//! still apply inside them. The engine's own git runs under such a policy in
//! every session whose commands are sandboxed (`base::engine_exec`), so a
//! filter driver or other program a repository's configuration names runs
//! confined, writing at most the git directory and the worktrees directory
//! when it creates a worktree, and only the session's `TMPDIR` otherwise.
//!
//! # What a confined command may not read
//!
//! The policy's `deny_read` (the credential stores in the user's home, by
//! default) less its `allow_read`, with the targets of the links in a
//! denied directory that lead out of it; its `deny_read_exact` (what the
//! session's `Read` deny rules name) whatever `allow_read` says, each path
//! as named, no link in it followed; and whatever the policy says, the
//! credentials of `base::paths` (`CREDENTIALS`, placed on disk by
//! `SensitivePaths::credential_locations`): the state root's settings files
//! and MCP OAuth tokens, its own and every scene's, the daemons' socket
//! tokens under `daemon/auth/`, and the project's `.agix/settings.local.json`
//! and `.agix/mcp.local.json`. The state root's are also those of each of
//! the policy's `foreign_state_roots` — another instance's on this machine,
//! such as `~/.agix` seen from an embedder's `data_root` — none of which a
//! command can write in either, unless the working directory is inside it.
//! macOS writes a pattern for each; Linux covers the state roots and the
//! project's `.agix` with the mounts of `mask::mask_plan`, which a credential
//! written later does not climb out of, except where escape path 6 says; a
//! foreign state root's tmpfs is read-only.
//!
//! On Linux a denied directory is shut under an empty read-only tmpfs, so a
//! command can neither read nor write in it; macOS denies reads only.
//!
//! A policy whose `read` is `ReadScope::Only` narrows that further
//! (`read_scope`).
//!
//! # What a confined command may not connect to
//!
//! The daemon it runs under: a command connected to the daemon's socket would
//! be its host, free to add an MCP server that runs outside the sandbox, trust
//! its own project or answer its own permission prompts. So under every
//! policy, network open or not, a command cannot connect to a unix socket in
//! the state root (unless the project lives there), in a foreign state root,
//! or to one this process serves ([`fence_socket`], wherever `--socket` put
//! it). macOS refuses the
//! connect, by resolved path; Linux mounts `/dev/null` over the socket — over
//! each one in a state root and its scene directories, not the directories
//! themselves, which stay readable. macOS also refuses this process's TCP
//! and WebSocket ports ([`fence_port`]); on Linux only `--unshare-net` cuts
//! them off, and the egress relay will not forward to one. Under `Proxy` both
//! backends set the command's `HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY` and
//! `NO_PROXY`, in both cases, over what it inherits. The daemon refuses its own descendants on the unix socket as
//! well, which is what holds where no backend runs.
//!
//! **Behind the `sandbox` feature: off here, on in assembly and the daemon.** What it
//! constrains is the commands the *model* runs, which is defence in depth on a
//! machine whose owner is sitting at it and a boundary only where the agent
//! acts for someone else. Without the feature the backends are not compiled,
//! [`PlatformSandbox`] says so, and `sandbox.require_enforcement` refuses every
//! command rather than running it unconstrained — which is the same promise the
//! enforcement report always made, kept one step earlier.
//!
//! # Known escape paths
//!
//! This is a lightweight write restriction, not a security boundary. Known
//! ways out:
//!
//! 1. **Writable `/dev`**: the macOS profile allows `file-write*` to `/dev`,
//!    so a process can `mknod` a raw disk device and bypass file ACLs.
//! 2. **Inherited environment**: `LD_PRELOAD` / `DYLD_INSERT_LIBRARIES` are
//!    not cleared, so a dynamic library can be injected.
//! 3. **Absent git files on Linux, for a git outside the sandbox**: a mount
//!    cannot stand in for a file that does not exist without leaving an empty
//!    one behind, which git refuses to read. So a project with no `.git` yet
//!    can be given one, and a repository with no `commondir` can be given one,
//!    with whatever configuration and filter drivers the command likes. The
//!    engine's own git runs inside the sandbox, so it carries none of that out
//!    unconfined; a git the user runs in the project afterwards does. macOS
//!    denies both.
//! 4. **Another process acting for the command**: the sandbox follows the
//!    command's own process tree, not what it asks a system service to do.
//!    On macOS, measured against `sandbox-exec`: `open` has LaunchServices
//!    start an app outside the sandbox, `defaults write` has `cfprefsd` write
//!    any preference domain, and an Apple event reaches an app that runs
//!    unconfined (Finder itself refuses events from a sandboxed sender, other
//!    apps need not). launchd refuses `launchctl submit` / `bootstrap` from a
//!    sandboxed caller. A policy with `deny_ipc_escapes` — every unattended
//!    session's — denies `lsopen`, `user-preference-write` and
//!    `appleevent-send`, the keychain through `com.apple.SecurityServer`
//!    (what `security find-generic-password` asks), and the sockets of the
//!    user's tmux and screen sessions. On Linux the same policy adds
//!    `--unshare-ipc` and hides the session bus and the systemd user
//!    manager's socket under `$XDG_RUNTIME_DIR` (and the bus path
//!    `DBUS_SESSION_BUS_ADDRESS` names), so `systemd-run --user` has nothing
//!    to talk to, and screen's socket directory (tmux's is in `/tmp`, private
//!    already); an abstract-socket bus is reachable unless the network is cut
//!    too. A multiplexer on a socket of its own choosing (`tmux -S`) is not
//!    found. With the network open, any local
//!    service listening on a socket the command can reach (sshd, a container
//!    daemon) is a way out under every policy.
//! 5. **A nested repository planted in the project**: the guard protects the
//!    project's own `.git`, but a command can `git init` a repository in a
//!    subdirectory, write its `.git/config` (`core.fsmonitor` and the like)
//!    and `git add` it into the project's index as a gitlink. The user's own
//!    `git` in the project afterwards, outside the sandbox, enters it and runs
//!    what its configuration names. Neither backend stops this: refusing to
//!    create a `.git` entry would break `git init`, `git clone` and every
//!    package manager that checks out a git dependency, and a repository built
//!    elsewhere and moved in would evade it anyway. The engine's own `git`
//!    carries `-c core.fsmonitor=false` and is unaffected. Above the backends,
//!    the runtime looks for one after every command not held to the strict
//!    profile (`tools::nested_repos`; a background command when its task
//!    ends): an undeclared gitlink whose configuration names a program has its
//!    `.git` moved aside unless the person attending keeps it. What it does
//!    not catch is what a command starts that outlives it and plants one
//!    later.
//! 6. **A credential written later directly into a writable directory, on
//!    Linux**: a `/dev/null` bind is pinned to the file there at wrap time, so
//!    a key file the engine creates afterwards, or rename-writes over the old
//!    one, is readable by what a command left running. A tmpfs over the
//!    directory would close this, and swallow every new entry the project
//!    makes in it. That is the state root's own key files, and `daemon/auth`
//!    while it does not exist, when the project is the state root; a scene's
//!    key files, and its `mcp/oauth` while it does not exist, when the
//!    project is that scene directory; and the key files of a project whose
//!    `.agix` is a symbolic link. What lies outside the project, the
//!    configuration directories a command cannot write anyway and a real
//!    `.agix` directory are masked whole. macOS matches its denials by path
//!    and has no such gap.
//! 7. **A `deny_read` entry inside a writable directory, on macOS**: matched
//!    by path, so a command that may write the directory renames the file,
//!    or a directory above it, and reads it under the new name. The
//!    `Read(...)` deny rules in `deny_read_exact` mostly name files in the
//!    project, which is writable. On Linux the entry is a mount, which a
//!    rename does not carry off.
//!
//! Not among them, measured on bubblewrap 0.8: `/proc` is mounted afresh for
//! the command's PID namespace, so it shows neither the host's processes nor
//! their `fd` and `root`; a command inherits no descriptor of the daemon's but
//! its stdio, the daemon opening every one close-on-exec; and it has no
//! controlling terminal to push input into through `/dev/tty`, since
//! `LocalProcess` starts it in a session of its own.

pub use base::interface::exec::{
    default_deny_read, default_deny_read_in, Confined, EgressProxy, Enforcement, EnvScope,
    NetworkMode, ProcessSpec, ReadScope, Sandbox, SandboxMode, SandboxPolicy,
};
use std::path::{Path, PathBuf};

#[cfg(all(feature = "sandbox", any(target_os = "macos", target_os = "linux")))]
mod egress;
mod fence;
#[cfg(all(feature = "sandbox", target_os = "linux"))]
mod linux;
#[cfg(all(feature = "sandbox", target_os = "macos"))]
mod macos;
#[cfg(feature = "sandbox")]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod mask;
#[cfg(all(feature = "sandbox", target_os = "macos"))]
mod patterns;
#[cfg(all(feature = "sandbox", any(target_os = "macos", target_os = "linux")))]
mod read_scope;
#[cfg(feature = "sandbox")]
mod roots;
#[cfg(test)]
mod tests;

pub use fence::{fence_port, fence_secret, fence_socket, Fence};

/// The system directories programs load from: what a command reads to
/// start at all, and what `ReadScope::Only` lets it read whatever its roots.
#[cfg(target_os = "macos")]
const SYSTEM_READ_ROOTS: &[&str] = &[
    "/usr",
    "/bin",
    "/sbin",
    "/System",
    "/Library/Developer/CommandLineTools",
    "/Applications/Xcode.app/Contents/Developer",
    "/private/etc",
    "/private/var/db/timezone",
    "/opt/homebrew",
    "/usr/local",
    "/dev",
];

#[cfg(target_os = "linux")]
const SYSTEM_READ_ROOTS: &[&str] = &[
    "/usr",
    "/bin",
    "/sbin",
    "/lib",
    "/lib32",
    "/lib64",
    "/libx32",
    "/etc",
    "/opt",
    "/nix/store",
];

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
const SYSTEM_READ_ROOTS: &[&str] = &[];

pub fn system_read_roots() -> Vec<PathBuf> {
    SYSTEM_READ_ROOTS.iter().map(PathBuf::from).collect()
}

/// Where a confined command may write besides its working directory and the
/// policy's `additional_writable`: device nodes, and on macOS the per-user
/// temporary directory, where mktemp(1) creates its file whatever `TMPDIR`
/// says — without it every script calling a bare `mktemp` fails. That one is
/// left out when only the named roots are writable.
pub fn implicit_writable_roots(only_additional: bool) -> Vec<PathBuf> {
    #[cfg_attr(not(all(feature = "sandbox", target_os = "macos")), allow(unused_mut))]
    let mut roots = vec![PathBuf::from("/dev")];
    #[cfg(all(feature = "sandbox", target_os = "macos"))]
    if !only_additional {
        roots.extend(macos::darwin_user_temp_dir());
    }
    #[cfg(not(all(feature = "sandbox", target_os = "macos")))]
    let _ = only_additional;
    roots
}

#[derive(Debug, Clone)]
pub struct SandboxOptions<'a> {
    pub command: &'a str,
    pub cwd: &'a Path,
    pub additional_writable: &'a [PathBuf],
    pub disable: bool,
    /// Extended policy (deny-read, network mode, etc).
    /// Falls back to safe defaults via `SandboxPolicy::default()`.
    pub policy: SandboxPolicy,
}

/// Pick the platform backend and wrap the command.
///
/// Never fails: no available backend means no constraint, reported honestly
/// through [`Enforcement`] rather than by refusing here. Whether an
/// unconstrained run is acceptable is the caller's decision, not this
/// function's.
pub fn wrap(opts: SandboxOptions<'_>) -> Confined {
    let inner = bash_spec(opts.command);
    if opts.disable {
        return plain(inner, SandboxMode::Disabled);
    }
    let env = CommandEnv {
        set: &[],
        scope: opts.policy.env,
    };
    wrap_program(inner, opts.cwd, opts.additional_writable, &opts.policy, env)
}

/// The environment a confined program starts with, as far as the sandbox can
/// tell: what its spec sets, over what `scope` lets it inherit from the
/// daemon's.
#[derive(Debug, Clone, Copy)]
#[cfg_attr(
    not(all(feature = "sandbox", any(target_os = "macos", target_os = "linux"))),
    allow(dead_code)
)]
struct CommandEnv<'a> {
    set: &'a [(String, String)],
    scope: EnvScope,
}

/// [`wrap`] for any program, not only a shell command.
#[cfg_attr(
    not(all(feature = "sandbox", any(target_os = "macos", target_os = "linux"))),
    allow(unused_variables)
)]
fn wrap_program(
    inner: ProcessSpec,
    cwd: &Path,
    additional_writable: &[PathBuf],
    policy: &SandboxPolicy,
    env: CommandEnv<'_>,
) -> Confined {
    #[cfg(not(feature = "sandbox"))]
    {
        unavailable(
            inner,
            "this build has no sandbox backend; rebuild with `--features sandbox`",
        )
    }

    #[cfg(feature = "sandbox")]
    {
        if let Some(odd) = non_utf8_path(cwd, additional_writable, policy) {
            return Confined {
                spec: inner,
                unmet: vec![format!(
                    "{} is not valid UTF-8, which a sandbox profile cannot name",
                    odd.display()
                )],
                mode: SandboxMode::Unavailable,
                enforcement: Enforcement::None,
            };
        }
        #[cfg(target_os = "macos")]
        {
            macos::mac_wrap(inner, cwd, additional_writable, policy, env)
        }

        #[cfg(target_os = "linux")]
        {
            linux::linux_wrap(inner, cwd, additional_writable, policy, env)
        }

        // Windows / other unsupported platforms
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            plain(inner, SandboxMode::Unavailable)
        }
    }
}

/// The first path of the call that is not UTF-8. Profiles and bwrap
/// arguments are text: such a path would be written lossily, as one that
/// does not exist.
#[cfg(feature = "sandbox")]
fn non_utf8_path<'a>(
    cwd: &'a Path,
    additional_writable: &'a [PathBuf],
    policy: &'a SandboxPolicy,
) -> Option<&'a Path> {
    let roots = match &policy.read {
        ReadScope::Everything => &[][..],
        ReadScope::Only(roots) => roots,
    };
    std::iter::once(cwd)
        .chain(additional_writable.iter().map(PathBuf::as_path))
        .chain(policy.additional_writable.iter().map(PathBuf::as_path))
        .chain(policy.deny_read.iter().map(PathBuf::as_path))
        .chain(policy.deny_read_exact.iter().map(PathBuf::as_path))
        .chain(policy.allow_read.iter().map(PathBuf::as_path))
        .chain(policy.state_root.iter().map(PathBuf::as_path))
        .chain(roots.iter().map(PathBuf::as_path))
        .find(|p| p.to_str().is_none())
}

/// A command nothing wrapped, and the reason in words the operator can act on.
#[cfg(not(feature = "sandbox"))]
fn unavailable(inner: ProcessSpec, why: &str) -> Confined {
    Confined {
        spec: inner,
        unmet: vec![why.to_string()],
        mode: SandboxMode::Unavailable,
        enforcement: Enforcement::None,
    }
}

fn plain(inner: ProcessSpec, mode: SandboxMode) -> Confined {
    Confined {
        spec: inner,
        unmet: vec!["nothing is constrained".into()],
        mode,
        // Nothing wraps this command, so nothing constrains it — true whether
        // the user asked for no sandbox or the platform could not provide one.
        // Which of those it was is `mode`'s job to say.
        enforcement: Enforcement::None,
    }
}

/// macOS sandbox-exec 的 TinyScheme 字符串里 `\` 和 `"` 要转义。
///
/// 只有 macOS 的配置生成器调用它，但函数本身和平台无关——它的测试在哪个平台上
/// 都该跑，所以这里没有跟着调用方一起 `cfg` 掉，而是只在别的平台上豁免"未使用"。
#[cfg(feature = "sandbox")]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn sandbox_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Where the terminal multiplexers this user runs listen. Typing into one of
/// their sessions runs a command in a shell outside the sandbox.
#[cfg(all(feature = "sandbox", any(target_os = "macos", target_os = "linux")))]
fn multiplexer_paths() -> Vec<PathBuf> {
    let uid = unsafe { libc::getuid() };
    let tmux_dir = format!("tmux-{uid}");
    let var = |name: &str| {
        std::env::var_os(name)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    let mut paths: Vec<PathBuf> = if cfg!(target_os = "macos") {
        vec![
            Path::new("/private/tmp").join(&tmux_dir),
            "/private/tmp/screens".into(),
        ]
    } else {
        vec!["/run/screen".into(), "/var/run/screen".into()]
    };
    paths.extend(var("TMUX_TMPDIR").map(|d| d.join(&tmux_dir)));
    paths.extend(var("SCREENDIR"));
    // `$TMUX` is `<socket>,<server pid>,<session>`.
    if let Ok(tmux) = std::env::var("TMUX") {
        paths.extend(
            tmux.split(',')
                .next()
                .filter(|s| !s.is_empty())
                .map(PathBuf::from),
        );
    }
    let mut paths: Vec<PathBuf> = paths
        .iter()
        .map(|p| base::path::resolve_physically(p))
        .collect();
    paths.sort();
    paths.dedup();
    paths
}

/// A shell command as a process spec. The one place `bash -c` is spelled, so
/// that every backend wraps the same thing.
fn bash_spec(command: &str) -> ProcessSpec {
    ProcessSpec::new("bash", std::path::Path::new("."))
        .args(["-c".to_string(), command.to_string()])
}

/// The platform backends behind the contract.
///
/// Holds nothing: which backend applies is a fact about the machine, decided
/// per call by `cfg`, and the policy arrives as an argument.
pub struct PlatformSandbox;

impl Sandbox for PlatformSandbox {
    fn confine(&self, spec: ProcessSpec, policy: &SandboxPolicy) -> Confined {
        let ProcessSpec {
            program,
            args,
            cwd,
            env,
            env_scope,
            stdin,
        } = spec;
        let inner = ProcessSpec::new(program, Path::new(".")).args(args);
        let env_scope = match policy.env {
            EnvScope::Minimal => EnvScope::Minimal,
            EnvScope::Session => env_scope,
        };
        let seen = CommandEnv {
            set: &env,
            scope: env_scope,
        };
        let mut confined = wrap_program(inner, &cwd, &policy.additional_writable, policy, seen);
        #[cfg(all(feature = "sandbox", any(target_os = "macos", target_os = "linux")))]
        if matches!(
            confined.mode,
            SandboxMode::MacOSSandboxExec | SandboxMode::LinuxBwrap
        ) {
            let unmet = read_scope::unmet(
                &policy.read,
                &read_scope::UserDirs::of(seen),
                read_scope::var(seen, "PATH").as_deref(),
            );
            if !unmet.is_empty() {
                confined.unmet.extend(unmet);
                confined.enforcement = Enforcement::Partial;
            }
        }
        confined.spec.cwd = cwd;
        let backend_env = std::mem::replace(&mut confined.spec.env, env);
        confined.spec.env.extend(backend_env);
        confined.spec.env_scope = env_scope;
        confined.spec.stdin = stdin;
        confined
    }
}

/// 一行可读的沙盒可用性描述，给 `/doctor` 用。
/// macOS：恒"available: sandbox-exec"（系统自带）。
/// Linux：bwrap 在 PATH 时 available；否则报 unavailable + 提示装。
/// 其它平台：unavailable + 平台名。
pub fn sandbox_status() -> String {
    #[cfg(not(feature = "sandbox"))]
    {
        "unavailable: this build has no sandbox backend (`--features sandbox`)".to_string()
    }
    #[cfg(all(feature = "sandbox", target_os = "macos"))]
    {
        "available: sandbox-exec".to_string()
    }
    #[cfg(all(feature = "sandbox", target_os = "linux"))]
    {
        if linux::bwrap_available() {
            "available: bwrap".to_string()
        } else {
            "unavailable: bwrap not in PATH (install bubblewrap)".to_string()
        }
    }
    #[cfg(all(
        feature = "sandbox",
        not(any(target_os = "macos", target_os = "linux"))
    ))]
    {
        format!("unavailable on {}", std::env::consts::OS)
    }
}
