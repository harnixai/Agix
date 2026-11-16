//! The Linux backend: wraps a command in `bwrap` mounts for a policy. bwrap has no deny
//! rule, only mounts applied in argument order, so every protection is a bind,
//! tmpfs or `/dev/null` laid over the path it guards.

use super::fence::{fenced_secrets, fenced_sockets, is_socket};
use super::roots::{
    denied_for_write, git_configured, link_targets_outside, project_agix_dirs, subdirs,
    submodules_of, write_denial_names, Roots,
};
use super::{
    egress, mask, multiplexer_paths, plain, read_scope, CommandEnv, Confined, Enforcement,
    NetworkMode, ProcessSpec, SandboxMode, SandboxPolicy,
};
use base::paths::{
    ENGINE_CONFIG_DIRS, ENGINE_CONFIG_FILES, ENGINE_CONFIG_ROOTS, GIT_CONTROL_FILES, GIT_HOOKS_DIR,
    PROJECT_ENGINE_CONFIG_DIRS, REPOSITORY_CONFIG_DIR, STATE_ROOT_SCENE_FILES,
    STATE_ROOT_WORKING_DIRS,
};
use std::path::{Path, PathBuf};

mod worktrees;

pub(super) fn bind(args: &mut Vec<String>, how: &str, path: &Path) {
    let p = path.display().to_string();
    args.extend([how.to_string(), p.clone(), p]);
}

/// Mount `path` read-only. One that does not exist yet gets an empty
/// read-only tmpfs instead, which is what keeps it from being created; bwrap
/// leaves the empty mount point behind, so this happens only where the user
/// could have created it anyway.
fn read_only(args: &mut Vec<String>, path: &Path) {
    if path.symlink_metadata().is_ok() {
        bind(args, "--ro-bind", path);
    } else if path.parent().is_some_and(writable_by_us) {
        let p = path.display().to_string();
        args.extend(["--tmpfs".into(), p.clone(), "--remount-ro".into(), p]);
    }
}

fn writable_by_us(dir: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c) = std::ffi::CString::new(dir.as_os_str().as_bytes()) else {
        return false;
    };
    unsafe { libc::access(c.as_ptr(), libc::W_OK) == 0 }
}

/// The Linux shape of [`project_guard`].
///
/// bwrap has no deny rule, only mounts, and a read-only bind of a file that
/// does not exist yet is skipped rather than refused. So `.agix` — and
/// `.claude` where it exists — is mounted read-only as a whole and its
/// working entries are opened again one by one (a real `.agix` directory is
/// held by [`mask_project_agix`] instead, to the same effect);
/// a configuration file that is not there yet cannot be created because its
/// directory is read-only. `.git` cannot be treated that way — git creates
/// its lock files directly in it — so it is bound onto itself, which makes it
/// a mount point that cannot be renamed or removed, and its control files are
/// mounted read-only where they exist. Each submodule's git directory, and
/// every directory on the way down to it, is bound onto itself the same way,
/// so that none of them can be moved aside for one with other contents.
///
/// A `.git` or `.agix` no writable root reaches is read-only already, and is
/// left alone: binding it writable would open it, and a mount point for an
/// absent entry cannot be made on a read-only parent.
fn guard_project(args: &mut Vec<String>, roots: &Roots, agix_masked: bool) {
    let dot_git = roots.cwd.join(REPOSITORY_CONFIG_DIR);
    let git = match &roots.worktree_git {
        Some(linked) => {
            if dot_git.symlink_metadata().is_ok_and(|m| m.is_file()) {
                bind(args, "--ro-bind", &dot_git);
            }
            worktrees::guard_linked(args, roots, linked);
            linked.dir.clone()
        }
        None => dot_git,
    };
    if git.is_dir() {
        let open = roots.opens(&git);
        let submodules = submodules_of(&git);
        if open {
            bind(args, "--bind", &git);
            for dir in &submodules.dirs {
                bind(args, "--bind", dir);
            }
        }
        let configured = git_configured(&roots.cwd, &git);
        if let Some(hooks) = configured.hooks.filter(|p| roots.opens(p)) {
            read_only(args, &hooks);
        }
        for file in configured.included.iter().filter(|p| roots.opens(p)) {
            bind(args, "--ro-bind-try", file);
        }
        for repo in std::iter::once(&git).chain(&submodules.git_dirs) {
            for f in GIT_CONTROL_FILES {
                bind(args, "--ro-bind-try", &repo.join(f));
            }
            worktrees::guard_entries(args, roots, repo, open, *repo == git);
            if open {
                read_only(args, &repo.join(GIT_HOOKS_DIR));
            } else {
                bind(args, "--ro-bind-try", &repo.join(GIT_HOOKS_DIR));
            }
        }
    } else if git.exists() {
        bind(args, "--ro-bind", &git);
    }

    for name in ENGINE_CONFIG_ROOTS {
        let root = roots.cwd.join(name);
        // Holding an absent root leaves an empty mount point behind in the
        // project, which for `.agix` is the engine's own and for `.claude`
        // would be litter in every project.
        let held = *name == ".agix" || root.symlink_metadata().is_ok();
        if !held || !roots.opens(&root) || (*name == ".agix" && agix_masked) {
            continue;
        }
        read_only(args, &root);
        for entry in working_entries(&root) {
            bind(args, "--bind", &entry);
        }
    }
    guard_project_skills(args, roots);
    worktrees::guard_links(args, roots);
}

fn guard_project_skills(args: &mut Vec<String>, roots: &Roots) {
    for dir in PROJECT_ENGINE_CONFIG_DIRS {
        let path = roots.cwd.join(dir);
        let above: Vec<PathBuf> = Path::new(dir)
            .ancestors()
            .skip(1)
            .filter(|a| !a.as_os_str().is_empty())
            .map(|a| roots.cwd.join(a))
            .collect();
        if !roots.opens(&path) {
            continue;
        }
        // Where a directory above it is missing, the topmost missing one is
        // shut under an empty read-only tmpfs, as macOS refuses to create it:
        // otherwise a command makes `.agents/skills/<name>` in a project that
        // has no `.agents` yet.
        if let Some(missing) = above.iter().rev().find(|a| a.symlink_metadata().is_err()) {
            read_only(args, missing);
            continue;
        }
        let held = above
            .iter()
            .all(|a| a.symlink_metadata().is_ok_and(|m| m.is_dir()));
        if !held {
            continue;
        }
        for a in above.iter().rev() {
            bind(args, "--bind", a);
        }
        read_only(args, &path);
    }
}

/// The entries of a configuration root `root` that are not configuration
/// and not symbolic links: what a command may write in it.
fn working_entries(root: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|entry| {
            let configures = entry.file_name().to_str().is_some_and(|n| {
                ENGINE_CONFIG_FILES.contains(&n) || ENGINE_CONFIG_DIRS.contains(&n)
            });
            !configures && entry.file_type().is_ok_and(|t| !t.is_symlink())
        })
        .map(|entry| entry.path())
        .collect()
}

/// The project's `.agix`, where it is a real directory in the sandbox,
/// masked around the project's key files as the state root is — a key file
/// the engine writes or rename-writes there later lands under a read-only
/// tmpfs — with the rest bound back: its working entries and the writable
/// roots in it writable where they are writable at all, everything else
/// read-only, and nothing new creatable directly in it, as
/// [`guard_project`] holds it otherwise. Whether it was masked.
fn mask_project_agix(
    args: &mut Vec<String>,
    roots: &Roots,
    secrets: &[base::paths::Located],
    read: &read_scope::LinuxReadScope<'_>,
) -> bool {
    let agix = roots.cwd.join(".agix");
    if !read.shows(&agix) || !agix.symlink_metadata().is_ok_and(|m| m.is_dir()) {
        return false;
    }
    let working = if roots.opens(&agix) {
        working_entries(&agix)
    } else {
        Vec::new()
    };
    let holes: Vec<PathBuf> = working
        .into_iter()
        .chain(
            roots
                .writable
                .iter()
                .filter(|p| p.starts_with(&agix) && **p != agix)
                .cloned(),
        )
        .collect();
    let plan = mask::mask_plan(&agix, secrets, &holes);
    apply_mask(args, &plan, true);
    !plan.is_empty()
}

/// The state root inside the sandbox: masked around its credentials, its
/// working directories (or the project in it) writable again.
fn expose_state_root(
    args: &mut Vec<String>,
    roots: &Roots,
    secrets: &[base::paths::Located],
    read: &read_scope::LinuxReadScope<'_>,
) {
    match (
        roots.fenced_state_root(),
        mask::holes_in_state_root(roots, secrets, |root| read.shows(root)),
    ) {
        (Some(root), _) if root.is_dir() => {
            expose_state_root_masked(args, root, secrets, &[]);
            for dir in STATE_ROOT_WORKING_DIRS {
                bind(args, "--bind-try", &root.join(dir));
            }
            for dir in roots.session_temp_dirs_in(root) {
                bind(args, "--bind-try", dir);
            }
        }
        // The project lives in the state root, or nothing but the named
        // roots is writable: the root is masked as above, the project and
        // the writable roots inside it bound back over it. A scene directory
        // opened as a project keeps its scene files read-only.
        (_, Some((root, holes))) => {
            expose_state_root_masked(args, root, secrets, &holes);
            for file in state_root_scene_files(roots) {
                if roots.opens(&file) && !roots.paths.is_credential(&file) {
                    bind(args, "--ro-bind-try", &file);
                }
            }
        }
        // The project is the state root itself, or the read scope leaves
        // the root out: a tmpfs over the root would mask the project's new
        // entries or put the root in the sandbox. Its configuration
        // directories, read-only anyway, are masked on their own; the key
        // files directly in it and `daemon/auth` are bound shut one by one,
        // and a key file the engine rename-writes there afterwards is
        // readable (the module header's escape path 6).
        _ => expose_unfenced_state_root(args, roots, secrets, read),
    }
}

/// The per-file shape of [`expose_state_root`], for a state root that is
/// the project or outside the read scope.
fn expose_unfenced_state_root(
    args: &mut Vec<String>,
    roots: &Roots,
    secrets: &[base::paths::Located],
    read: &read_scope::LinuxReadScope<'_>,
) {
    let Some(state_root) = &roots.state_root else {
        return;
    };
    // A path the read scope leaves out is not in the sandbox at all:
    // binding it read-only would put it there.
    let (config_files, config_dirs) = roots.unfenced_state_root_config();
    for file in config_files.iter().filter(|p| read.shows(p)) {
        bind(args, "--ro-bind-try", file);
    }
    let mut masked = Vec::new();
    for dir in config_dirs.iter().filter(|p| read.shows(p)) {
        let plan = mask::mask_below(state_root, secrets, dir);
        if plan.is_empty() {
            read_only(args, dir);
        } else {
            apply_mask(args, &plan, true);
            masked.push(dir);
        }
    }
    let unmasked = |p: &Path| !masked.iter().any(|dir| p.starts_with(dir));
    for file in state_root_scene_files(roots) {
        if read.shows(&file) && unmasked(&file) {
            bind(args, "--ro-bind-try", &file);
        }
    }
    for p in secrets.iter().flat_map(|l| l.existing()) {
        if unmasked(&p) {
            hide(args, &p);
        }
    }
}

/// `STATE_ROOT_SCENE_FILES` in the state root and in each of
/// its scene directories as they are now.
fn state_root_scene_files(roots: &Roots) -> Vec<PathBuf> {
    roots
        .state_root
        .iter()
        .cloned()
        .chain(roots.paths.scene_dirs())
        .flat_map(|dir| {
            STATE_ROOT_SCENE_FILES
                .iter()
                .map(move |file| dir.join(file))
        })
        .collect()
}

/// Expose the state root read-only while keeping its credentials masked
/// against writes that land after the sandbox is set up: the mounts of
/// [`mask::mask_plan`].
///
/// A `/dev/null` bind over a key file is pinned to that file's dentry. The
/// engine rewrites `settings.local.json` through a temporary file and a
/// rename, which replaces the dentry and drops the bind (measured on bwrap
/// 0.8), and a key file not there at wrap is never bound at all — so a
/// lingering process from an earlier command could read a credential written
/// afterwards. Under the plan's tmpfs a credential created or
/// rename-rewritten afterwards lands on no path the command can read.
///
/// The readable contents enumerated from the host are bound back as they are
/// at wrap time; a readable file created later does not appear, which for a
/// confined command reading the state root is acceptable where masking a late
/// credential is not.
fn expose_state_root_masked(
    args: &mut Vec<String>,
    root: &Path,
    secrets: &[base::paths::Located],
    holes: &[PathBuf],
) {
    apply_mask(args, &mask::mask_plan(root, secrets, holes), false);
}

/// `plan` as bwrap arguments; `read_only` remounts each of its tmpfs
/// read-only once everything is bound onto it, so that nothing new can be
/// created directly in a masked directory.
fn apply_mask(args: &mut Vec<String>, plan: &[mask::Mount], read_only: bool) {
    for mount in plan {
        match mount {
            mask::Mount::Tmpfs(at) => args.extend(["--tmpfs".into(), at.display().to_string()]),
            mask::Mount::RoBind(at) => bind(args, "--ro-bind-try", at),
            mask::Mount::Bind(at) => bind(args, "--bind-try", at),
            mask::Mount::Hide(at) => hide(args, at),
        }
    }
    if read_only {
        for mount in plan {
            if let mask::Mount::Tmpfs(at) = mount {
                args.extend(["--remount-ro".into(), at.display().to_string()]);
            }
        }
    }
}

/// Make `path` unreadable where it exists: an empty tmpfs over a directory,
/// `/dev/null` over anything else.
pub(super) fn hide(args: &mut Vec<String>, path: &Path) {
    if path.is_dir() {
        args.extend(["--tmpfs".into(), path.display().to_string()]);
    } else if path.exists() {
        bind_over(args, Path::new("/dev/null"), path);
    }
}

pub(super) fn bind_over(args: &mut Vec<String>, source: &Path, target: &Path) {
    args.extend([
        "--ro-bind-try".to_string(),
        source.display().to_string(),
        target.display().to_string(),
    ]);
}

/// Where this user's session D-Bus and systemd user manager listen. An
/// abstract socket (`unix:abstract=`) has no path to hide; only
/// `--unshare-net` reaches it.
pub(super) fn session_bus_paths() -> Vec<PathBuf> {
    bus_paths(
        std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from),
        std::env::var("DBUS_SESSION_BUS_ADDRESS").ok().as_deref(),
    )
}

pub(super) fn bus_paths(runtime_dir: Option<PathBuf>, bus_address: Option<&str>) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(runtime) = runtime_dir {
        paths.push(runtime.join("bus"));
        paths.push(runtime.join("systemd"));
    }
    if let Some(address) = bus_address {
        paths.extend(
            address
                .split(';')
                .flat_map(|a| a.split(','))
                .filter_map(|kv| kv.strip_prefix("unix:path=").or(kv.strip_prefix("path=")))
                .map(PathBuf::from),
        );
    }
    paths.sort();
    paths.dedup();
    paths
}

/// The unix sockets a command must not reach: those this process serves, and
/// any in the state root or one of its scene directories, where daemons put
/// theirs. bwrap cannot refuse a connect, only hide the file.
pub(super) fn daemon_sockets(state_root: Option<&Path>) -> Vec<PathBuf> {
    let mut sockets = fenced_sockets();
    if let Some(root) = state_root {
        let dirs = std::iter::once(root.to_path_buf()).chain(subdirs(&root.join("scenes")));
        for dir in dirs {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            sockets.extend(entries.flatten().map(|e| e.path()).filter(|p| is_socket(p)));
        }
    }
    sockets.sort();
    sockets.dedup();
    sockets
}

/// Runs bwrap (`"$@"`) with the command's stdout and stderr on fds 3 and 4,
/// bwrap's own stdout on `/dev/null` and its stderr on an unlinked scratch
/// file printed to stderr once bwrap exits; stdin (fd 0) passes through to
/// bwrap and the command untouched. bwrap's PID 1 keeps fds 0–2 for as long
/// as anything the command left in the background runs, which would hold the
/// command's output open; it closes every other fd.
///
/// The scratch file is made in the host's `/tmp`, not the session `TMPDIR`
/// that `mktemp` would otherwise pick up from the environment: the session
/// `TMPDIR` is bound writable into the sandbox, so a process an earlier
/// command left running there could swap the file for a symlink between
/// `mktemp` and the `exec` that opens it, and this wrapper runs outside the
/// sandbox. The sandbox always mounts its own tmpfs over `/tmp`, so a file in
/// the host's `/tmp` is on no path a confined command can reach. `mktemp`,
/// `rm` and `cat` are addressed absolutely so a writable directory earlier on
/// `PATH` cannot stand one of them in.
pub(super) fn bwrap_stdio_outside() -> String {
    let mktemp = trusted_bin("mktemp");
    let rm = trusted_bin("rm");
    let cat = trusted_bin("cat");
    format!(
        "err=$({mktemp} -p /tmp 2>/dev/null) && exec 5>\"$err\" 6<\"$err\" && {rm} -f \"$err\" || exec 5>/dev/null 6</dev/null\n\
         \"$@\" 3>&1 4>&2 >/dev/null 2>&5 5>&- 6<&-\n\
         status=$?\n\
         {cat} <&6 >&2\n\
         exit \"$status\""
    )
}

/// The absolute path of a core utility `name`, looked up in the system
/// directories rather than `PATH` (which a command could have planted a
/// writable entry on). Falls back to the bare name where none is found, so a
/// deployment with its utilities elsewhere still runs, unhardened.
pub(super) fn trusted_bin(name: &str) -> String {
    find_trusted_bin(name).unwrap_or_else(|| name.to_string())
}

/// [`trusted_bin`] without the fallback.
pub(super) fn find_trusted_bin(name: &str) -> Option<String> {
    ["/usr/bin", "/bin", "/usr/sbin", "/sbin"]
        .iter()
        .map(|dir| Path::new(dir).join(name))
        .find(|path| path.exists())
        .map(|path| path.display().to_string())
}

/// The inside half of [`BWRAP_STDIO_OUTSIDE`]: fds 3 and 4 back to 1 and 2,
/// then the command.
const BWRAP_STDIO_INSIDE: &str = r#"exec 1>&3 2>&4 3>&- 4>&- && exec "$@""#;

pub(super) fn linux_wrap(
    inner: ProcessSpec,
    cwd: &Path,
    additional_writable: &[PathBuf],
    policy: &SandboxPolicy,
    env: CommandEnv<'_>,
) -> Confined {
    if !bwrap_available() {
        return plain(inner, SandboxMode::Unavailable);
    }
    let roots = Roots::resolve(cwd, additional_writable, policy);
    // bwrap applies mounts in argument order, so each one below lands on top
    // of the ones before it.
    let read = read_scope::LinuxReadScope::new(&policy.read, read_scope::UserDirs::of(env));
    let mut args: Vec<String> = Vec::new();
    read.base_mounts(&mut args);
    namespace_args(&mut args);
    // After the `/tmp` tmpfs, or a project under `/tmp` is hidden by it.
    read.scope_mounts(&mut args);
    bind_writable_roots(&mut args, &roots, &read);
    protect_denied_writes(&mut args, &roots, &policy.deny_write);
    hide_project_and_state(&mut args, &roots, &read);
    hide_fenced(&mut args, &roots);
    hide_denied_reads(&mut args, policy);
    isolate_ipc(&mut args, policy);
    let proxy = network_args(&mut args, policy);
    args.push("--".into());

    let mut unmet: Vec<String> = if policy.network_mode == NetworkMode::Allowlist {
        vec!["domain allowlist: bwrap can only cut the network entirely".into()]
    } else {
        Vec::new()
    };
    let (inside, env) = match proxy {
        Some(proxy) => {
            unmet.extend(proxy.unmet);
            (proxy.inside, proxy.env)
        }
        None => (None, Vec::new()),
    };
    args.extend(["sh".into(), "-c".into()]);
    args.push(inside.unwrap_or_else(|| BWRAP_STDIO_INSIDE.to_string()));
    args.push("sh".into());
    args.push(inner.program);
    args.extend(inner.args);
    let outside = [
        "-c".into(),
        bwrap_stdio_outside(),
        "sh".into(),
        "bwrap".into(),
    ];
    Confined {
        enforcement: if unmet.is_empty() {
            Enforcement::Full
        } else {
            Enforcement::Partial
        },
        unmet,
        spec: ProcessSpec {
            env,
            ..ProcessSpec::new("sh", &inner.cwd).args(outside.into_iter().chain(args))
        },
        mode: SandboxMode::LinuxBwrap,
    }
}

fn namespace_args(args: &mut Vec<String>) {
    args.extend(
        [
            "--tmpfs",
            "/tmp",
            "--proc",
            "/proc",
            "--dev",
            "/dev",
            // No `--die-with-parent`: bwrap exits as soon as the command does,
            // and the namespace would go with it, taking whatever the command
            // left in the background. That stays in the command's process
            // group, which is how it gets stopped.
            //
            // No `--new-session` either: bwrap's PID 1 would take the whole
            // namespace out of that group, out of reach of stopping it. The
            // `TIOCSTI` it guards against needs a controlling terminal, and
            // `LocalProcess` starts every command in a session without one.
            "--unshare-pid",
        ]
        .map(String::from),
    );
}

fn bind_writable_roots(
    args: &mut Vec<String>,
    roots: &Roots,
    read: &read_scope::LinuxReadScope<'_>,
) {
    if !roots.only_additional {
        bind(args, "--bind", &roots.cwd);
    } else if roots.cwd.starts_with("/tmp") && read.shows(&roots.cwd) {
        bind(args, "--ro-bind-try", &roots.cwd);
    }
    // A worktree's git directory, read-only beneath the parts of it that
    // are writable, is in the sandbox even where `/tmp` would hide it;
    // its branch ref is opened by [`guard_project`].
    let mut opened_by_the_guard = Vec::new();
    if let Some(linked) = &roots.worktree_git {
        if read.shows(&linked.dir) {
            bind(args, "--ro-bind-try", &linked.dir);
        }
        opened_by_the_guard = vec![linked.branch_ref(), linked.branch_ref_lock()];
    }
    for p in &roots.writable {
        if !opened_by_the_guard.contains(p) {
            bind(args, "--bind-try", p);
        }
    }
}

/// The deepest a `deny_write` name is looked for below a writable root, and
/// the most entries looked at: a bound on what wrapping a command costs.
const DENY_WRITE_DEPTH: usize = 8;
const DENY_WRITE_ENTRIES: usize = 50_000;

/// Each existing entry of a writable root that a `deny_write` name matches
/// is bound read-only. bwrap can only mount over what is there, so a match
/// created after the command starts is not covered. Laid right after the
/// writable roots, so that the masks and tmpfs of the project guard, which
/// come later, still cover whatever lies below a matched entry.
fn protect_denied_writes(args: &mut Vec<String>, roots: &Roots, denied: &[String]) {
    let denied = write_denial_names(denied);
    if denied.is_empty() {
        return;
    }
    let mut looked_at = 0;
    for root in roots.write_roots() {
        let mut pending = vec![(root.to_path_buf(), 0)];
        while let Some((dir, depth)) = pending.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                looked_at += 1;
                if looked_at > DENY_WRITE_ENTRIES {
                    return;
                }
                let Ok(kind) = entry.file_type() else {
                    continue;
                };
                let name = entry.file_name();
                if name.to_str().is_some_and(|n| denied_for_write(n, &denied)) {
                    if !kind.is_symlink() {
                        bind(args, "--ro-bind", &entry.path());
                    }
                } else if kind.is_dir() && depth < DENY_WRITE_DEPTH && name != ".git" {
                    pending.push((entry.path(), depth + 1));
                }
            }
        }
    }
}

fn hide_project_and_state(
    args: &mut Vec<String>,
    roots: &Roots,
    read: &read_scope::LinuxReadScope<'_>,
) {
    let (state_root_secrets, project_secrets): (Vec<_>, Vec<_>) = roots
        .paths
        .credential_locations(&project_agix_dirs(&roots.cwd))
        .into_iter()
        .partition(|located| roots.state_root.as_ref() == Some(&located.base));
    expose_state_root(args, roots, &state_root_secrets, read);
    let agix_masked = mask_project_agix(args, roots, &project_secrets, read);
    guard_project(args, roots, agix_masked);
    if !agix_masked {
        for p in project_secrets.iter().flat_map(|l| l.existing()) {
            hide(args, &p);
        }
    }
}

fn hide_fenced(args: &mut Vec<String>, roots: &Roots) {
    for socket in daemon_sockets(roots.state_root.as_deref()) {
        bind_over(args, Path::new("/dev/null"), &socket);
    }
    for secret in fenced_secrets() {
        bind_over(args, Path::new("/dev/null"), &secret);
    }
}

fn hide_denied_reads(args: &mut Vec<String>, policy: &SandboxPolicy) {
    let allow_set: std::collections::HashSet<PathBuf> = policy.allow_read.iter().cloned().collect();
    for p in &policy.deny_read {
        if !allow_set.contains(p) {
            hide(args, p);
            for target in link_targets_outside(p) {
                hide(args, &target);
            }
        }
    }
}

fn isolate_ipc(args: &mut Vec<String>, policy: &SandboxPolicy) {
    if !policy.deny_ipc_escapes {
        return;
    }
    args.push("--unshare-ipc".into());
    // `/tmp` is the command's own tmpfs already.
    let multiplexers = multiplexer_paths()
        .into_iter()
        .filter(|p| !p.starts_with("/tmp"));
    for p in session_bus_paths().into_iter().chain(multiplexers) {
        hide(args, &p);
    }
}

/// The proxy's relay, under `NetworkMode::Proxy`. The relay's binds land
/// after the `/tmp` tmpfs, which would cover them otherwise.
fn network_args(args: &mut Vec<String>, policy: &SandboxPolicy) -> Option<egress::Namespaced> {
    // bwrap's only network lever is `--unshare-net`. An allowlist it cannot
    // honour fails closed to no network rather than open to all of it; stricter
    // than asked for in one direction and not what was asked for in the other,
    // which is what `Partial` is for.
    match policy.network_mode {
        NetworkMode::Unrestricted => {}
        NetworkMode::Proxy => {
            args.push("--unshare-net".into());
            let socat = find_trusted_bin("socat");
            let programs = egress::Programs {
                socat: socat.as_deref(),
                grep: &trusted_bin("grep"),
                sleep: &trusted_bin("sleep"),
            };
            let proxy = egress::namespaced(policy.egress_proxy.as_ref(), &programs, egress::relay);
            args.extend(proxy.args.iter().cloned());
            return Some(proxy);
        }
        NetworkMode::DenyAll => {
            args.push("--unshare-net".into());
        }
        NetworkMode::Allowlist => {
            tracing::warn!(
                domains = ?policy.allowed_domains,
                "sandbox: NetworkMode::Allowlist has no bwrap equivalent (no domain/IP \
                 filtering primitive) — falling back to full network isolation \
                 (--unshare-net) rather than allowing unrestricted access"
            );
            args.push("--unshare-net".into());
        }
    }
    None
}

pub(super) fn bwrap_available() -> bool {
    // 一次性探测；用 sync std::process::Command 走 PATH lookup
    static AVAILABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        std::process::Command::new("bwrap")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    })
}
