#[cfg(feature = "sandbox")]
use super::fence::*;
#[cfg(all(feature = "sandbox", target_os = "linux"))]
use super::linux::*;
#[cfg(all(feature = "sandbox", target_os = "macos"))]
use super::macos::*;
#[cfg(feature = "sandbox")]
use super::*;
#[cfg(all(feature = "sandbox", target_os = "macos"))]
use base::paths::{STATE_ROOT_SESSION_TMP, STATE_ROOT_WORKING_DIRS};

#[cfg(feature = "sandbox")]
mod credentials;
#[cfg(all(feature = "sandbox", any(target_os = "macos", target_os = "linux")))]
mod egress;
mod enforcement;
#[cfg(feature = "sandbox")]
mod fence;
#[cfg(all(feature = "sandbox", target_os = "linux"))]
mod linux;
#[cfg(all(feature = "sandbox", target_os = "macos"))]
mod macos;
#[cfg(all(feature = "sandbox", target_os = "macos"))]
mod macos_live;
#[cfg(feature = "sandbox")]
mod roots;
#[cfg(feature = "sandbox")]
mod strict;
#[cfg(not(feature = "sandbox"))]
mod without_feature;
#[cfg(feature = "sandbox")]
mod wrap;

#[cfg(feature = "sandbox")]
#[cfg(target_os = "macos")]
const INHERITED: CommandEnv<'static> = CommandEnv {
    set: &[],
    scope: EnvScope::Session,
};

#[cfg(feature = "sandbox")]
fn opts<'a>(cmd: &'a str, cwd: &'a Path) -> SandboxOptions<'a> {
    SandboxOptions {
        command: cmd,
        cwd,
        additional_writable: &[],
        disable: false,
        policy: SandboxPolicy::default(),
    }
}

#[cfg(feature = "sandbox")]
/// A project and state root reached through `/tmp`, which macOS resolves
/// to `/private/tmp` — the alias a profile written against the given path
/// would never match — and a temp dir standing in for the session's.
#[cfg(target_os = "macos")]
struct UnderSandboxExec {
    _dirs: Vec<tempfile::TempDir>,
    project: PathBuf,
    state_root: PathBuf,
    tmp: PathBuf,
    elsewhere: PathBuf,
    layout: base::paths::Layout,
}

#[cfg(feature = "sandbox")]
#[cfg(target_os = "macos")]
#[allow(clippy::unwrap_used)]
impl UnderSandboxExec {
    fn new() -> Option<Self> {
        Self::in_layout(base::paths::Layout::default())
    }

    /// A project of an instance whose project directory is `layout`'s, with
    /// `.agix` and `.claude` beside it.
    fn in_layout(layout: base::paths::Layout) -> Option<Self> {
        let works = std::process::Command::new("sandbox-exec")
            .args(["-p", "(version 1)(allow default)", "true"])
            .status()
            .is_ok_and(|s| s.success());
        if !works {
            return None;
        }
        let dirs: Vec<tempfile::TempDir> = (0..4)
            .map(|_| tempfile::Builder::new().tempdir_in("/tmp").unwrap())
            .collect();
        let [project, state_root, tmp, elsewhere] =
            [0, 1, 2, 3].map(|i| dirs[i].path().to_path_buf());
        assert!(project.starts_with("/tmp"));
        std::fs::create_dir_all(project.join(".agix")).unwrap();
        std::fs::create_dir_all(project.join(".claude")).unwrap();
        std::fs::create_dir_all(layout.project_state_dir(&project)).unwrap();
        for dir in STATE_ROOT_WORKING_DIRS {
            std::fs::create_dir_all(state_root.join(dir)).unwrap();
        }
        std::fs::write(project.join("a"), "a\n").unwrap();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(&project)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}: {out:?}");
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "T"]);
        Some(Self {
            _dirs: dirs,
            project,
            state_root,
            tmp,
            elsewhere,
            layout,
        })
    }

    fn runs(&self, command: &str) -> bool {
        self.run(command, false)
    }

    fn runs_unattended(&self, command: &str) -> bool {
        self.run(command, true)
    }

    fn runs_allowing(&self, domains: &[&str], command: &str) -> bool {
        self.run_under(
            command,
            SandboxPolicy {
                network_mode: NetworkMode::Allowlist,
                allowed_domains: domains.iter().map(|d| d.to_string()).collect(),
                ..Default::default()
            },
        )
    }

    fn run(&self, command: &str, deny_ipc_escapes: bool) -> bool {
        self.run_under(
            command,
            SandboxPolicy {
                deny_ipc_escapes,
                ..Default::default()
            },
        )
    }

    fn run_under(&self, command: &str, policy: SandboxPolicy) -> bool {
        self.run_at(&self.project, &[], command, policy)
    }

    /// `command` as a sub-agent in the worktree `.wkts/<slug>`,
    /// on branch `<slug>`, runs it: writing what a commit there needs of the
    /// project's git directory.
    fn runs_in_worktree(&self, slug: &str, command: &str) -> bool {
        let git = worktree_git(&self.project, slug);
        self.run_at(
            &self.project.join(".wkts").join(slug),
            &git.writable(),
            command,
            SandboxPolicy {
                worktree_git: Some(git),
                ..Default::default()
            },
        )
    }

    /// `command` with `cwd` its working directory, writing `additional` too.
    fn run_at(
        &self,
        cwd: &Path,
        additional: &[PathBuf],
        command: &str,
        policy: SandboxPolicy,
    ) -> bool {
        let additional: Vec<PathBuf> = std::iter::once(self.tmp.clone())
            .chain(additional.iter().cloned())
            .collect();
        let confined = wrap(SandboxOptions {
            command,
            cwd,
            additional_writable: &additional,
            disable: false,
            policy: SandboxPolicy {
                state_root: Some(self.state_root.clone()),
                layout: self.layout.clone(),
                ..policy
            },
        });
        std::process::Command::new(&confined.spec.program)
            .args(&confined.spec.args)
            .current_dir(cwd)
            .env("TMPDIR", &self.tmp)
            .output()
            .unwrap()
            .status
            .success()
    }
}

#[cfg(feature = "sandbox")]
#[cfg(target_os = "macos")]
fn outside(program: &str, args: &[&str]) -> bool {
    std::process::Command::new(program)
        .args(args)
        .output()
        .is_ok_and(|o| o.status.success())
}

#[cfg(feature = "sandbox")]
/// A unix socket that answers every connection and hangs up.
#[cfg(target_os = "macos")]
#[allow(clippy::unwrap_used)]
fn listening(path: PathBuf) -> PathBuf {
    let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            drop(conn);
        }
    });
    path
}

#[cfg(feature = "sandbox")]
/// Whether this machine has a backend that actually runs. Where it has
/// none the real-run cases below skip, saying why, unless
/// `AGIX_REQUIRE_SANDBOX` is set — CI sets it, so that a runner whose
/// bubblewrap cannot make namespaces fails rather than passes empty.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn a_backend_runs() -> bool {
    #[cfg(target_os = "macos")]
    let works = std::process::Command::new("sandbox-exec")
        .args(["-p", "(version 1)(allow default)", "true"])
        .status()
        .is_ok_and(|s| s.success());
    #[cfg(target_os = "linux")]
    let works = bwrap_available()
        && std::process::Command::new("bwrap")
            .args(["--ro-bind", "/", "/", "true"])
            .status()
            .is_ok_and(|s| s.success());
    if !works {
        assert!(
            std::env::var_os("AGIX_REQUIRE_SANDBOX").is_none(),
            "AGIX_REQUIRE_SANDBOX is set and no sandbox backend runs here"
        );
        eprintln!(
            "skipped: no sandbox backend runs here (Linux: install bubblewrap and \
                 allow unprivileged user namespaces)"
        );
    }
    works
}

#[cfg(feature = "sandbox")]
/// `command` under the real backend, in `cwd`, with `env` added.
#[cfg(any(target_os = "macos", target_os = "linux"))]
#[allow(clippy::unwrap_used)]
fn run_confined(
    cwd: &Path,
    policy: &SandboxPolicy,
    env: &[(&str, &Path)],
    command: &str,
) -> std::process::Output {
    let confined = PlatformSandbox.confine(
        ProcessSpec::new("bash", cwd).args(["-c".to_string(), command.to_string()]),
        policy,
    );
    assert_eq!(
        confined.enforcement,
        Enforcement::Full,
        "{:?}",
        confined.unmet
    );
    let mut run = std::process::Command::new(&confined.spec.program);
    run.args(&confined.spec.args).current_dir(cwd);
    for (k, v) in env {
        run.env(k, v);
    }
    run.output().unwrap()
}

#[cfg(feature = "sandbox")]
#[cfg(any(target_os = "macos", target_os = "linux"))]
#[allow(clippy::unwrap_used)]
fn write_file(path: &Path, body: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
}

/// What the engine finds of `project`'s git directory for its worktree
/// `.wkts/<slug>` on branch `<slug>`.
#[cfg(feature = "sandbox")]
#[cfg(unix)]
fn worktree_git(project: &Path, slug: &str) -> base::paths::WorktreeGit {
    let dir = project.join(".git");
    base::paths::WorktreeGit {
        admin: dir.join("worktrees").join(slug),
        dir,
        branch: slug.to_string(),
    }
}
