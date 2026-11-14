//! How a session reaches the machine: its default providers, its own
//! temporary directory, and the engine's git — made once when the session is
//! built and handed to every call.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use base::context::EngineConfig;
use base::engine_exec::EngineExec;
use base::interface::exec::{ConfinementReport, ExecProviders, SandboxSettings};

/// The execution providers a session's tool calls reach the machine through,
/// with every process group a command starts recorded in `process_groups`.
///
/// Only the egress is configured from `EngineConfig`; the other three are
/// this machine either way. `sandbox.network_mode` is a statement about where
/// the *model* may reach, which is as true of a `WebFetch` as it is of a
/// `curl` inside a sandboxed command.
pub(crate) fn providers_for(
    config: &EngineConfig,
    process_groups: Option<&Arc<platform::GroupLedger>>,
) -> ExecProviders {
    let env = config.subprocess_env.clone();
    match process_groups {
        Some(ledger) => platform::local_providers_with_ledger(env, ledger.clone()),
        None => platform::local_providers_with(env),
    }
    .with_network(Arc::new(platform::LocalNetwork::for_agent_policy(
        config.sandbox.network_mode,
        config.sandbox.allowed_domains.clone(),
    )))
}

/// How the engine runs its own git and helpers for the session `session_id`:
/// in its sandbox whenever its commands are, with the session's temporary
/// directory made ready.
pub(crate) fn engine_git_for(
    config: &EngineConfig,
    exec: &ExecProviders,
    session_id: &str,
) -> EngineExec {
    let tmpdir = if config.sandbox.disabled {
        None
    } else {
        prepare_session_tmp(&config.sandbox, session_id)
    };
    config
        .sandbox
        .engine_git(exec, config.project_trust, tmpdir)
}

/// What the session's sandbox holds a command to, probed the first time a
/// call of the session asks.
pub(crate) fn confinement_of(
    session: &base::context::SessionState,
    config: &EngineConfig,
    exec: &ExecProviders,
    cwd: &Path,
    session_id: &str,
) -> ConfinementReport {
    session.confinement(|| config.sandbox.confinement(exec, cwd, session_id))
}

/// How long a session's temporary directory may go untouched before another
/// session's start clears it away.
const STALE_SESSION_TMP: Duration = Duration::from_secs(7 * 24 * 3600);

/// [`SandboxSettings::session_tmp_dir`], created private to this user if it
/// is not there yet.
pub(crate) fn prepare_session_tmp(settings: &SandboxSettings, session_id: &str) -> Option<PathBuf> {
    let dir = settings.session_tmp_dir(session_id);
    let private = |recursive| {
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(recursive);
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        builder
    };
    if let Some(parent) = dir
        .parent()
        .filter(|_| settings.paths.state_root().is_some())
    {
        let _ = private(true).create(parent);
        clear_stale_session_tmp_dirs(parent);
    }
    match private(false).create(&dir) {
        Ok(()) => Some(dir),
        Err(e)
            if e.kind() == std::io::ErrorKind::AlreadyExists
                && dir.symlink_metadata().is_ok_and(|m| m.is_dir()) =>
        {
            Some(dir)
        }
        Err(e) => {
            tracing::warn!(dir = %dir.display(), error = %e, "could not create the session's TMPDIR");
            None
        }
    }
}

/// Once per process: the session directories under `parent` nothing has
/// touched for [`STALE_SESSION_TMP`]. Nothing else removes them — unlike the
/// system temporary directory, the state root is not swept by the OS.
fn clear_stale_session_tmp_dirs(parent: &Path) {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let Ok(entries) = std::fs::read_dir(parent) else {
            return;
        };
        let now = std::time::SystemTime::now();
        for entry in entries.flatten() {
            let stale = entry.metadata().is_ok_and(|m| {
                m.is_dir()
                    && m.modified().is_ok_and(|t| {
                        now.duration_since(t).unwrap_or_default() > STALE_SESSION_TMP
                    })
            });
            if stale {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn under(state: &Path) -> EngineConfig {
        let mut config = EngineConfig::defaults_for("test-model");
        config.sandbox = Arc::new(SandboxSettings {
            paths: Arc::new(base::paths::SensitivePaths::new(Some(state), None)),
            ..Default::default()
        });
        config
    }

    #[test]
    fn each_session_gets_its_own_private_tmpdir_under_the_state_root() {
        let state = tempfile::tempdir().unwrap();
        let config = under(state.path());
        let exec = base::interface::exec::ExecProviders::in_process();
        let a = engine_git_for(&config, &exec, "session-a");
        let b = engine_git_for(&config, &exec, "session-b");
        let root = state.path().join(base::paths::STATE_ROOT_SESSION_TMP);
        assert_eq!(a.tmpdir(), Some(root.join("session-a").as_path()));
        assert_eq!(b.tmpdir(), Some(root.join("session-b").as_path()));
        let a = a.tmpdir().unwrap();
        assert!(a.is_dir());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(a.metadata().unwrap().permissions().mode() & 0o777, 0o700);
        }
    }

    #[test]
    fn a_session_without_a_sandbox_makes_no_tmpdir() {
        let state = tempfile::tempdir().unwrap();
        let mut config = under(state.path());
        Arc::make_mut(&mut config.sandbox).disabled = true;
        let exec = base::interface::exec::ExecProviders::in_process();
        let git = engine_git_for(&config, &exec, "session-a");
        assert!(!git.is_sandboxed());
        assert_eq!(git.tmpdir(), None);
        assert!(!state
            .path()
            .join(base::paths::STATE_ROOT_SESSION_TMP)
            .exists());
    }

    /// The `settings.json` egress knob reaches the network a tool's own HTTP
    /// goes out through, not only the sandbox wrapped around a subprocess.
    #[test]
    fn the_egress_is_taken_from_the_engine_config() {
        use base::interface::exec::{NetworkMode, Origin};
        let mut config = EngineConfig::defaults_for("test-model");
        let sandbox = Arc::make_mut(&mut config.sandbox);
        sandbox.network_mode = NetworkMode::Allowlist;
        sandbox.allowed_domains = vec!["api.example.com".into()];

        let exec = providers_for(&config, None);

        assert!(exec.network.permits("api.example.com", Origin::Agent));
        assert!(
            !exec.network.permits("evil.test", Origin::Agent),
            "a configured allowlist has to reach the tools' egress"
        );
        assert!(
            exec.network.permits("evil.test", Origin::Operator),
            "and stop at what the model chose, not at what the deployment did"
        );
    }
}
