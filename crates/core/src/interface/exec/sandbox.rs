//! `Sandbox` — how a process is constrained.
//!
//! The split this contract exists to keep: a backend decides *how* to
//! constrain, the kernel decides *what* to constrain and *whether to at all*.
//! The policy below is the kernel's; an implementation may only report how
//! much of it it managed to deliver.
//!
//! **The hard invariant: a policy that asked for constraint must never
//! silently become an unconstrained run.** A backend that cannot deliver says
//! so through [`Enforcement`], and the caller — with
//! `sandbox.require_enforcement` — decides whether to refuse. A `warn!` nobody
//! reads is not a decision.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use super::env::EnvScope;
use super::process::ProcessSpec;

/// What to constrain. The kernel's, not a backend's.
///
/// Defaults bake in a deny-read list (`~/.ssh`, `~/.aws`, …) so that a naive
/// Bash command does not dump credential files into the model's transcript by
/// accident. Not every field lands cleanly on every backend; the ones that do
/// not are reported through [`Enforcement`] rather than ignored.
#[derive(Debug, Clone, Default)]
pub struct SandboxPolicy {
    /// Absolute paths the sandbox is allowed to **read**, on top of the
    /// universal default (everything readable). An entry that is also in
    /// `deny_read` is re-allowed; a parent directory, or a path below a
    /// denied one, changes nothing.
    pub allow_read: Vec<PathBuf>,
    /// Absolute paths the sandbox is **denied** read access to. Defaults
    /// include common credential stores (see [`default_deny_read`]).
    pub deny_read: Vec<PathBuf>,
    /// Filename segments no command may write inside a writable root: a path
    /// is refused when a component under the root equals one or begins with
    /// it followed by a dot. macOS refuses new and existing paths alike;
    /// Linux can only mount over the entries that exist when the command
    /// starts.
    pub deny_write: Vec<String>,
    /// Network policy — see [`NetworkMode`].
    pub network_mode: NetworkMode,
    /// Domains allowed to make outbound connections when `network_mode` is
    /// [`NetworkMode::Allowlist`]. Ignored otherwise. What each backend makes
    /// of it is under [`NetworkMode::Allowlist`].
    pub allowed_domains: Vec<String>,
    /// The only place outbound connections may go under
    /// [`NetworkMode::Proxy`]. Ignored otherwise.
    pub egress_proxy: Option<EgressProxy>,
    /// Paths outside `cwd` this command may still write to.
    ///
    /// Part of the policy rather than a separate argument because it answers
    /// the same question the rest of it does — what may be written — and a
    /// backend that received it separately could be handed a policy that
    /// looks complete and is not.
    pub additional_writable: Vec<PathBuf>,
    /// Nothing is writable but `additional_writable` and `/dev`: not `cwd`,
    /// not the state root's working directories, not the per-user temporary
    /// directory. For the engine's own git, which reads the project and
    /// writes only where its bookkeeping lives.
    pub only_additional_writable: bool,
    /// Where a working directory that is a linked worktree commits to, as
    /// the engine checked it when it made the worktree: its git directory is
    /// guarded as `<cwd>/.git` is otherwise, and what of it is writable is
    /// [`crate::paths::WorktreeGit::writable`] in `additional_writable`. The
    /// backends never follow the worktree's own links to find it, since a
    /// command may have rewritten them.
    pub worktree_git: Option<crate::paths::WorktreeGit>,
    /// This instance's global state root. `None` falls back to `$HOME/.agix`,
    /// which is where an instance usually lives but not where it must: a
    /// redirected instance whose sandbox protected `$HOME/.agix` would be
    /// guarding a file it does not use while leaving its real settings.json
    /// writable.
    pub state_root: Option<PathBuf>,
    /// Also refuse the ways a command can have another process act for it
    /// outside the sandbox: on macOS sending Apple events, opening apps and
    /// URLs through LaunchServices, and writing preferences through
    /// `cfprefsd`; on Linux the host's IPC namespace and the session bus.
    /// For an unattended session, where nobody approved the command.
    pub deny_ipc_escapes: bool,
    /// What may be read at all; `deny_read` and the engine's credentials
    /// stay unreadable inside it.
    pub read: ReadScope,
    /// The environment the command starts from. The backend marks the
    /// confined spec with it ([`ProcessSpec::env_scope`]); the process
    /// provider applies it.
    pub env: EnvScope,
}

/// What a confined command may read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum ReadScope {
    #[default]
    Everything,
    /// These roots, and those of the backend's own table: the system
    /// directories programs load from and the user's git configuration.
    /// Anywhere else a command sees names and metadata but no contents.
    Only(Vec<PathBuf>),
}

impl ReadScope {
    /// Whether `path` is under one of the caller's roots. The backend's own
    /// table is not consulted.
    pub fn names(&self, path: &Path) -> bool {
        match self {
            Self::Everything => true,
            Self::Only(roots) => roots.iter().any(|root| path.starts_with(root)),
        }
    }
}

impl SandboxPolicy {
    /// The policy for a command let through on its own word
    /// (`ExecProfile::Strict`): no network, nothing writable but `tmpdir`,
    /// nothing readable but `readable` and `tmpdir`, the minimal
    /// environment, and the IPC routes out refused. What this policy keeps
    /// from being read stays unreadable under the strict one.
    pub fn strict(
        &self,
        readable: impl IntoIterator<Item = PathBuf>,
        tmpdir: Option<PathBuf>,
    ) -> Self {
        let writable: Vec<PathBuf> = tmpdir.into_iter().collect();
        Self {
            allow_read: self.allow_read.clone(),
            deny_read: self.deny_read.clone(),
            deny_write: self.deny_write.clone(),
            network_mode: NetworkMode::DenyAll,
            allowed_domains: Vec::new(),
            egress_proxy: None,
            read: ReadScope::Only(
                readable
                    .into_iter()
                    .chain(writable.iter().cloned())
                    .collect(),
            ),
            additional_writable: writable,
            only_additional_writable: true,
            worktree_git: self.worktree_git.clone(),
            state_root: self.state_root.clone(),
            deny_ipc_escapes: true,
            env: EnvScope::Minimal,
        }
    }
}

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Default,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum NetworkMode {
    /// Unrestricted outbound (default; Bash often needs `curl` / `npm install`).
    #[default]
    Unrestricted,
    /// No outbound network access at all. On macOS 26 that includes name
    /// resolution, which goes through `mDNSResponder`'s unix socket.
    DenyAll,
    /// Outbound restricted to `SandboxPolicy::allowed_domains`. Neither
    /// backend can filter by domain: macOS lets through localhost when the
    /// list names it and nothing else, Linux cuts the network entirely, and
    /// both report the domains they could not allow as unmet.
    Allowlist,
    /// Outbound only to [`SandboxPolicy::egress_proxy`], with the command's
    /// `*_PROXY` variables pointing at it; what the proxy lets through is the
    /// proxy's business. Without an `egress_proxy` the network is cut, as
    /// under `DenyAll`, and reported unmet.
    Proxy,
}

/// `sandbox.egress_proxy`: the proxy a sandboxed command's traffic must go
/// through. Addresses, never names, so that nothing has to be resolved to
/// write the rule or the variables.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EgressProxy {
    /// An HTTP proxy that takes `CONNECT`.
    pub http: SocketAddr,
    /// A SOCKS5 proxy, for `ALL_PROXY`.
    pub socks: Option<SocketAddr>,
    /// `NO_PROXY`. Empty overrides whatever the command would inherit, so
    /// even `localhost` goes through the proxy.
    pub no_proxy: Vec<String>,
}

impl EgressProxy {
    pub fn addresses(&self) -> impl Iterator<Item = SocketAddr> + '_ {
        std::iter::once(self.http).chain(self.socks)
    }
}

/// The engine's sandbox settings for a session, as `settings.json`'s
/// `sandbox` section gave them: what a command is held to, before any one
/// call narrows it. Every [`SandboxPolicy`] a command or the engine's own git
/// runs under is derived from this, here.
#[derive(Debug, Clone, Default)]
pub struct SandboxSettings {
    pub allow_read: Vec<PathBuf>,
    /// Empty means the built-in credential list ([`default_deny_read`]); a
    /// list replaces it whole.
    pub deny_read: Vec<PathBuf>,
    pub network_mode: NetworkMode,
    pub allowed_domains: Vec<String>,
    pub egress_proxy: Option<EgressProxy>,
    /// Filename segments no tool may write, on top of the built-in ones;
    /// sandboxed commands are held to them too ([`SandboxPolicy::deny_write`]).
    pub deny_write: Vec<String>,
    /// Paths exempt from the write deny rules.
    pub allow_write: Vec<PathBuf>,
    /// This instance's sensitive paths: its state root, so the profile
    /// protects the settings files that actually govern it, and the
    /// credentials the gate and the file tools refuse.
    pub paths: std::sync::Arc<crate::paths::SensitivePaths>,
    pub require_enforcement: bool,
    pub unattended_requires_network_policy: bool,
    /// `dangerously_disable_sandbox`: commands run outside the OS sandbox.
    pub disabled: bool,
    /// [`SandboxPolicy::worktree_git`], for a session working in a linked
    /// worktree.
    pub worktree_git: Option<crate::paths::WorktreeGit>,
}

impl SandboxSettings {
    /// The policy a session's command runs under before its call adds what
    /// it may write.
    pub fn policy(&self) -> SandboxPolicy {
        let deny_read = if self.deny_read.is_empty() {
            default_deny_read()
        } else {
            self.deny_read.clone()
        };
        SandboxPolicy {
            allow_read: self.allow_read.clone(),
            deny_read,
            deny_write: self.deny_write.clone(),
            network_mode: self.network_mode,
            allowed_domains: self.allowed_domains.clone(),
            egress_proxy: self.egress_proxy.clone(),
            state_root: self.paths.state_root().map(Path::to_path_buf),
            additional_writable: Vec::new(),
            only_additional_writable: false,
            worktree_git: self.worktree_git.clone(),
            deny_ipc_escapes: false,
            read: ReadScope::Everything,
            env: EnvScope::Session,
        }
    }

    /// The session's own temporary directory: a sandboxed command's `TMPDIR`,
    /// and the only temporary directory it may write besides the one macOS
    /// `mktemp` insists on. Not created here.
    ///
    /// Under the state root rather than in the system temporary directory: on
    /// macOS that is the per-user `DARWIN_USER_TEMP_DIR` every sandboxed
    /// command may write, so one session's commands could reach into
    /// another's.
    pub fn session_tmp_dir(&self, session_id: &str) -> PathBuf {
        self.paths.session_tmp_dir(session_id).unwrap_or_else(|| {
            let id: String = session_id
                .chars()
                .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
                .collect();
            std::env::temp_dir().join(format!("agix-{id}"))
        })
    }

    /// What the session's sandbox holds a command to, for the permission
    /// policy to read: one probe command confined the way a command nobody
    /// attends is, since that is the command the answer lets run. Only
    /// `exec.sandbox` is asked; nothing runs.
    pub fn confinement(
        &self,
        exec: &super::ExecProviders,
        cwd: &Path,
        session_id: &str,
    ) -> ConfinementReport {
        let (require, network) = (
            self.require_enforcement,
            self.unattended_requires_network_policy,
        );
        if self.disabled {
            return ConfinementReport::disabled(require, network);
        }
        let mut policy = self.policy();
        policy.additional_writable = vec![self.session_tmp_dir(session_id)];
        policy.deny_ipc_escapes = true;
        let probe = ProcessSpec::new("bash", cwd).args(["-c", "true"]);
        ConfinementReport::probe(&*exec.sandbox, probe, &policy, require, network)
    }

    /// How the engine starts its own git and helpers: through the session's
    /// providers, and inside its sandbox whenever the session's shell
    /// commands are, so what a repository such a command configured makes
    /// git run is held to the same roots. `tmpdir` is the session's, made
    /// ready.
    pub fn engine_git(
        &self,
        exec: &super::ExecProviders,
        trust: crate::permission::ProjectTrust,
        tmpdir: Option<PathBuf>,
    ) -> crate::engine_exec::EngineExec {
        use crate::engine_exec::{Confinement, EngineExec};
        let confinement = if self.disabled {
            Confinement::Unconfined
        } else {
            Confinement::Sandboxed {
                policy: self.policy(),
                require_enforcement: self.require_enforcement,
                tmpdir,
            }
        };
        EngineExec::new(exec.clone(), confinement, trust)
    }
}

/// Default deny-read paths — credential stores that almost never want to be
/// inside an LLM tool result. User can override via `sandbox.allow_read` in
/// settings.json. Returned absolute (`HOME` resolved).
///
/// 两个生产调用点：
/// - `SandboxSettings::policy` —— 空 `deny_read` 回落到这里，作为真正的沙盒 profile；
/// - `bash::classify` —— 命令分类时用同一份名单判断某个参数是不是在读凭据，
///   避免 `cat ~/.ssh/id_rsa` 被当成"只读命令"静默放行。
///   两处共用一份定义，是为了不让"沙盒挡得住"和"分类器放得过"两套标准打架。
pub fn default_deny_read() -> Vec<PathBuf> {
    crate::paths::home_dir()
        .map(|h| default_deny_read_in(&h))
        .unwrap_or_default()
}

/// [`default_deny_read`] for the home directory `home`.
pub fn default_deny_read_in(home: &Path) -> Vec<PathBuf> {
    crate::paths::HOME_CREDENTIAL_STORES
        .iter()
        .map(|p| home.join(p))
        .collect()
}

/// Which backend ran. Advisory — for logs and tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxMode {
    /// Explicitly turned off by configuration.
    Disabled,
    /// No backend for this platform, or its tool is missing.
    Unavailable,
    /// macOS `sandbox-exec`.
    MacOSSandboxExec,
    /// Linux `bubblewrap`.
    LinuxBwrap,
}

/// How much of the policy is actually in force.
///
/// This is the field a caller acts on, as against [`SandboxMode`], which only
/// names the backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Enforcement {
    /// Everything the policy asked for.
    Full,
    /// Some of it. The backend has capabilities the policy needs and lacks
    /// others — neither backend has a domain-level network filter, so an
    /// allowlist of domains becomes "no network" (localhost aside, on macOS),
    /// which is stricter in one direction and not what was asked for in the
    /// other. What is missing is listed in [`Confined::unmet`].
    Partial,
    /// None of it. The command below runs unconstrained.
    None,
}

/// A command, after the sandbox has had its say.
#[derive(Debug, Clone)]
pub struct Confined {
    /// What to actually run — usually the original program wrapped in the
    /// backend's own.
    pub spec: ProcessSpec,
    pub mode: SandboxMode,
    pub enforcement: Enforcement,
    /// The parts of the policy this backend could not deliver, one per line,
    /// in words a person can act on. Empty when `enforcement` is `Full`.
    pub unmet: Vec<String>,
}

/// How a process is constrained.
///
/// A pure transformation, which is what makes it testable: it turns the
/// command someone meant to run into the command that will actually run, and
/// reports honestly how much of the policy survived. It does not execute
/// anything — that is [`Process`](super::Process), and keeping them apart is
/// what lets a caller inspect the confinement before committing to it.
pub trait Sandbox: Send + Sync {
    fn confine(&self, spec: ProcessSpec, policy: &SandboxPolicy) -> Confined;
}

/// What the session's sandbox holds a command to, found once when the session
/// is built. The permission policy reads it and never probes the sandbox
/// itself.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ConfinementReport {
    /// How much of the session's policy holds a command.
    pub session: Enforcement,
    /// What of the session's policy does not hold, as [`Confined::unmet`].
    pub session_unmet: Vec<String>,
    /// How much of the strict policy ([`SandboxPolicy::strict`]) holds one.
    pub strict: Enforcement,
    /// What of the strict policy does not hold, as [`Confined::unmet`].
    pub strict_unmet: Vec<String>,
    /// The model cannot reach whatever host it likes.
    pub network_restricted: bool,
    pub require_enforcement: bool,
    pub unattended_requires_network_policy: bool,
}

impl Default for ConfinementReport {
    /// Nothing found out: no command is held.
    fn default() -> Self {
        Self {
            session: Enforcement::None,
            session_unmet: Vec::new(),
            strict: Enforcement::None,
            strict_unmet: Vec::new(),
            network_restricted: false,
            require_enforcement: false,
            unattended_requires_network_policy: false,
        }
    }
}

impl ConfinementReport {
    /// Confine a probe command under `policy`, and under its strict
    /// counterpart, and report how much of each held.
    pub fn probe(
        sandbox: &dyn Sandbox,
        probe: ProcessSpec,
        policy: &SandboxPolicy,
        require_enforcement: bool,
        unattended_requires_network_policy: bool,
    ) -> Self {
        let strict = sandbox.confine(probe.clone(), &policy.strict(Vec::new(), None));
        let session = sandbox.confine(probe, policy);
        Self {
            session: session.enforcement,
            session_unmet: session.unmet,
            strict: strict.enforcement,
            strict_unmet: strict.unmet,
            network_restricted: !matches!(policy.network_mode, NetworkMode::Unrestricted),
            require_enforcement,
            unattended_requires_network_policy,
        }
    }

    /// No sandbox at all: the session turned it off.
    pub fn disabled(require_enforcement: bool, unattended_requires_network_policy: bool) -> Self {
        Self {
            require_enforcement,
            unattended_requires_network_policy,
            ..Self::default()
        }
    }

    /// A command nobody can be asked about may run: the sandbox fully holds
    /// it, and the network is restricted when the settings ask for that too.
    pub fn holds_unattended(&self) -> bool {
        self.session == Enforcement::Full
            && (!self.unattended_requires_network_policy || self.network_restricted)
    }
}

#[cfg(test)]
mod settings_tests {
    use super::*;

    #[test]
    fn the_policy_carries_the_settings_through() {
        let settings = SandboxSettings {
            allow_read: vec![PathBuf::from("/home/u/.npmrc")],
            deny_read: vec![PathBuf::from("/tmp/secret")],
            allowed_domains: vec!["api.example.com".to_string()],
            network_mode: NetworkMode::Allowlist,
            ..Default::default()
        };
        let policy = settings.policy();
        assert_eq!(policy.allow_read, [PathBuf::from("/home/u/.npmrc")]);
        assert_eq!(policy.deny_read, [PathBuf::from("/tmp/secret")]);
        assert_eq!(policy.allowed_domains, ["api.example.com"]);
        assert_eq!(policy.network_mode, NetworkMode::Allowlist);
    }

    /// An empty `deny_read` is the built-in credential list, not "deny
    /// nothing"; `allow_read` re-allows on top of it and is not defaulted.
    #[test]
    fn an_empty_deny_read_is_the_credential_defaults() {
        let policy = SandboxSettings::default().policy();
        assert_eq!(policy.deny_read, default_deny_read());
        assert!(policy.allow_read.is_empty());
        if crate::paths::home_dir().is_some() {
            let joined = policy
                .deny_read
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join("|");
            assert!(joined.contains(".ssh") && joined.contains(".aws"));
        }
    }

    #[test]
    fn a_proxy_restricts_the_network_and_the_strict_policy_drops_it() {
        struct Holds;
        impl Sandbox for Holds {
            fn confine(&self, spec: ProcessSpec, _: &SandboxPolicy) -> Confined {
                Confined {
                    spec,
                    mode: SandboxMode::MacOSSandboxExec,
                    enforcement: Enforcement::Full,
                    unmet: Vec::new(),
                }
            }
        }
        let settings = SandboxSettings {
            network_mode: NetworkMode::Proxy,
            egress_proxy: Some(EgressProxy {
                http: "127.0.0.1:3128".parse().unwrap(),
                socks: None,
                no_proxy: Vec::new(),
            }),
            ..Default::default()
        };
        let policy = settings.policy();
        assert_eq!(policy.egress_proxy, settings.egress_proxy);
        let strict = policy.strict(Vec::new(), None);
        assert_eq!(strict.network_mode, NetworkMode::DenyAll);
        assert!(strict.egress_proxy.is_none());
        let probe = ProcessSpec::new("bash", Path::new("/work"));
        let report = ConfinementReport::probe(&Holds, probe, &policy, false, true);
        assert!(report.network_restricted);
        assert!(report.holds_unattended());
    }

    #[test]
    fn a_disabled_sandbox_confines_neither_commands_nor_the_engines_git() {
        let settings = SandboxSettings {
            disabled: true,
            ..Default::default()
        };
        let exec = super::super::ExecProviders::in_process();
        let report = settings.confinement(&exec, Path::new("/work"), "s");
        assert_eq!(report, ConfinementReport::disabled(false, false));
        let git = settings.engine_git(&exec, crate::permission::ProjectTrust::Trusted, None);
        assert!(!git.is_sandboxed());
    }
}
