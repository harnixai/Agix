//! The Linux backend: wraps a command in `bwrap` mounts for a policy. bwrap has no deny
//! rule, only mounts applied in argument order, so every protection is a bind,
//! tmpfs or `/dev/null` laid over the path it guards.

use super::fence::{fenced_secrets, fenced_sockets, is_socket};
use super::roots::{
    denied_for_write, git_configured, link_targets_outside, subdirs, submodules_of,
    write_denial_names, Roots,
};
use super::{
    egress, mask, multiplexer_paths, plain, read_scope, CommandEnv, Confined, Enforcement,
    NetworkMode, ProcessSpec, SandboxMode, SandboxPolicy,
};
use base::paths::{
    GIT_CONTROL_FILES, GIT_HOOKS_DIR, PROJECT_ENGINE_CONFIG_DIRS, REPOSITORY_CONFIG_DIR,
    STATE_ROOT_SCENE_FILES, STATE_ROOT_WORKING_DIRS,
};
use std::path::{Path, PathBuf};

mod held;
mod stdio;
mod worktrees;

use stdio::BWRAP_STDIO_INSIDE;
pub(super) use stdio::{
    bwrap_program, bwrap_stdio_outside, find_trusted_bin, trusted_bin, Cleanup,
};

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

/// Shut `path`, absent or an empty directory, under an empty read-only
/// tmpfs, as [`read_only`] shuts an absent one; anything else read-only.
/// Where bwrap has to make the mount point, or mounts over an empty
/// directory, the path goes on `made`, for the wrapper to remove once the
/// command is done if it is still empty ([`bwrap_stdio_outside`]): a foreign
/// root, `.agents` or worktrees directory held so leaves nothing behind.
fn shut_empty(args: &mut Vec<String>, path: &Path, made: &mut Vec<PathBuf>) {
    let empty_dir = path.symlink_metadata().is_ok_and(|m| m.is_dir())
        && std::fs::read_dir(path).is_ok_and(|mut d| d.next().is_none());
    let absent = path.symlink_metadata().is_err();
    if !(empty_dir || absent && path.parent().is_some_and(writable_by_us)) {
        read_only(args, path);
        return;
    }
    let p = path.display().to_string();
    args.extend(["--tmpfs".into(), p.clone(), "--remount-ro".into(), p]);
    made.push(path.to_path_buf());
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
/// does not exist yet is skipped rather than refused. So the project
/// directory — and each foreign configuration root, shut empty where it is
/// not there ([`shut_empty`]) — is
/// mounted read-only as a whole and its working entries are opened again one
/// by one (an engine directory that is a real directory is held by
/// [`mask_engine_dirs`] instead, to the same effect);
/// a configuration file that is not there yet cannot be created because its
/// directory is read-only. `.git` cannot be treated that way — git creates
/// its lock files directly in it — so it is bound onto itself, which makes it
/// a mount point that cannot be renamed or removed, and its control files are
/// mounted read-only where they exist. Each submodule's git directory, and
/// every directory on the way down to it, is bound onto itself the same way,
/// so that none of them can be moved aside for one with other contents.
///
/// A `.git` or configuration root no writable root reaches is read-only
/// already, and is left alone: binding it writable would open it, and a mount
/// point for an absent entry cannot be made on a read-only parent.
fn guard_project(
    args: &mut Vec<String>,
    roots: &Roots,
    masked: &[PathBuf],
    made: &mut Vec<PathBuf>,
) {
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

    for root in roots.paths.config_roots_in(&roots.cwd) {
        let root_dir = root.path.clone();
        if !roots.opens(&root_dir) || masked.contains(&root_dir) {
            continue;
        }
        // The instance's own directory, made here when absent, is the
        // engine's own and stays.
        if root.own {
            read_only(args, &root_dir);
        } else {
            shut_empty(args, &root_dir, made);
        }
        for entry in working_entries(roots, &root) {
            bind(args, "--bind-try", &entry);
        }
    }
    guard_project_skills(args, roots, made);
    worktrees::guard_links(args, roots, made);
}

fn guard_project_skills(args: &mut Vec<String>, roots: &Roots, made: &mut Vec<PathBuf>) {
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
            shut_empty(args, missing, made);
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
        shut_empty(args, &path, made);
    }
}

/// What a command may write in the configuration root `root`: the existing
/// directories in its `state/` that
/// [`base::paths::ConfigRootAt::writable_state_entry`] admits, neither they
/// nor `state/` symbolic links, and the writable roots inside it, which the
/// policy names.
fn working_entries(roots: &Roots, root: &base::paths::ConfigRootAt) -> Vec<PathBuf> {
    let state = root.state_dir();
    let existing = STATE_ROOT_WORKING_DIRS
        .iter()
        .filter(|name| root.writable_state_entry(name) && !state.is_symlink())
        .map(|name| state.join(name))
        .filter(|entry| entry.symlink_metadata().is_ok_and(|m| m.is_dir()));
    let named = roots
        .writable
        .iter()
        .filter(|w| w.starts_with(&root.path) || w.starts_with(&root.resolved))
        .cloned();
    existing.chain(named).collect()
}

/// The configuration root of the project that `dir` is, as given or as it
/// resolves.
fn config_root_at(roots: &Roots, dir: &Path) -> Option<base::paths::ConfigRootAt> {
    roots
        .paths
        .config_roots_in(&roots.cwd)
        .into_iter()
        .find(|root| root.path == dir || root.resolved == dir)
}

/// Each engine directory of the project ([`base::paths::SensitivePaths::engine_dirs_in`]),
/// where it is a real directory in the sandbox, masked around the project's
/// key files as the state root is — a key file the engine writes or
/// rename-writes there later lands under a read-only tmpfs — with the rest
/// bound back: its working entries and the writable roots in it writable
/// where they are writable at all, everything else read-only, and nothing
/// new creatable directly in it, as [`guard_project`] holds it otherwise.
/// The ones it masked.
fn mask_engine_dirs(
    args: &mut Vec<String>,
    roots: &Roots,
    secrets: &[base::paths::Located],
    read: &read_scope::LinuxReadScope<'_>,
) -> Vec<PathBuf> {
    roots
        .paths
        .engine_dirs_in(&roots.cwd)
        .into_iter()
        .filter(|dir| dir.parent() == Some(roots.cwd.as_path()))
        .filter(|dir| mask_engine_dir(args, roots, dir, secrets, read))
        .collect()
}

fn mask_engine_dir(
    args: &mut Vec<String>,
    roots: &Roots,
    dir: &Path,
    secrets: &[base::paths::Located],
    read: &read_scope::LinuxReadScope<'_>,
) -> bool {
    if !read.shows(dir) || !dir.symlink_metadata().is_ok_and(|m| m.is_dir()) {
        return false;
    }
    let working = match config_root_at(roots, dir) {
        Some(root) if roots.opens(dir) => working_entries(roots, &root),
        _ => Vec::new(),
    };
    let mut holes: Vec<PathBuf> = working
        .into_iter()
        .chain(
            roots
                .writable
                .iter()
                .filter(|p| p.starts_with(dir) && *p != dir)
                .cloned(),
        )
        .collect();
    holes.sort();
    holes.dedup();
    let plan = mask::mask_plan(dir, secrets, &holes);
    apply_mask(args, &plan, true);
    !plan.is_empty()
}

/// Each other instance's state root that is a real directory in the
/// sandbox and not masked already, masked around its credentials and
/// read-only, with what of the writable roots lies in it bound back
/// writable: a credential written there later lands under the mask, and
/// nothing new can be made directly in it. One a writable root cannot be
/// bound back into — the working directory is the root itself — is left to
/// the per-file hiding of what is there now. The ones it masked.
fn mask_foreign_state_roots(
    args: &mut Vec<String>,
    roots: &Roots,
    secrets: &[base::paths::Located],
    read: &read_scope::LinuxReadScope<'_>,
    masked: &[PathBuf],
) -> Vec<PathBuf> {
    let mut done = Vec::new();
    for root in &roots.foreign_state_roots {
        if masked.contains(root)
            || !read.shows(root)
            || !root.symlink_metadata().is_ok_and(|m| m.is_dir())
        {
            continue;
        }
        let holes: Vec<PathBuf> = roots
            .write_roots()
            .filter(|p| p.starts_with(root))
            .map(Path::to_path_buf)
            .collect();
        if !holes
            .iter()
            .all(|hole| mask::fits_a_hole(root, secrets, hole))
        {
            continue;
        }
        let plan = mask::mask_plan(root, secrets, &holes);
        apply_mask(args, &plan, true);
        if !plan.is_empty() {
            done.push(root.clone());
        }
    }
    done
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
    if inside_a_shut_directory(args, path) {
        return;
    }
    if path.is_dir() {
        args.extend(["--tmpfs".into(), path.display().to_string()]);
    } else if path.exists() {
        bind_over(args, Path::new("/dev/null"), path);
    }
}

/// [`hide`], with a directory's tmpfs read-only: what a command writes in a
/// directory it may not read fails, rather than landing where nobody sees it.
fn shut(args: &mut Vec<String>, path: &Path) {
    let before = args.len();
    hide(args, path);
    if args.len() > before && path.is_dir() {
        args.extend(["--remount-ro".into(), path.display().to_string()]);
    }
}

/// Whether `path` lies below a directory `args` already shut under an empty
/// read-only tmpfs that nothing was mounted into since: it is not there to
/// read, and bwrap could not make a mount point for it.
fn inside_a_shut_directory(args: &[String], path: &Path) -> bool {
    args.windows(4).enumerate().any(|(at, w)| {
        let dir = Path::new(&w[1]);
        w[0] == "--tmpfs"
            && w[2] == "--remount-ro"
            && w[1] == w[3]
            && path.starts_with(dir)
            && path != dir
            && !args[at + 4..].iter().any(|a| Path::new(a).starts_with(dir))
    })
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
/// any in a state root — the instance's own or another's — or one of its
/// scene directories, where daemons put theirs. bwrap cannot refuse a
/// connect, only hide the file.
pub(super) fn daemon_sockets<'a>(state_roots: impl IntoIterator<Item = &'a Path>) -> Vec<PathBuf> {
    let mut sockets = fenced_sockets();
    for root in state_roots {
        let dirs = std::iter::once(root.to_path_buf())
            .chain(subdirs(&base::paths::StateRoot::new(root).scenes_dir()));
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
    held::hold_read_only_dirs(&mut args, &roots);
    protect_denied_writes(&mut args, &roots, &policy.deny_write);
    let made = hide_project_and_state(&mut args, &roots, &read);
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
        bwrap_stdio_outside(&Cleanup {
            project: roots.cwd.clone(),
            made,
        }),
        "sh".into(),
        bwrap_program().to_string(),
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
            ..ProcessSpec::new(trusted_bin("sh"), &inner.cwd).args(outside.into_iter().chain(args))
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

/// The directories [`shut_empty`] made or covered, for the wrapper to remove.
fn hide_project_and_state(
    args: &mut Vec<String>,
    roots: &Roots,
    read: &read_scope::LinuxReadScope<'_>,
) -> Vec<PathBuf> {
    let (state_root_secrets, project_secrets): (Vec<_>, Vec<_>) = roots
        .paths
        .credential_locations(&roots.paths.engine_dirs_in(&roots.cwd))
        .into_iter()
        .partition(|located| roots.state_root.as_ref() == Some(&located.base));
    expose_state_root(args, roots, &state_root_secrets, read);
    let mut masked = mask_engine_dirs(args, roots, &project_secrets, read);
    masked.extend(mask_foreign_state_roots(
        args,
        roots,
        &project_secrets,
        read,
        &masked,
    ));
    let mut made = Vec::new();
    guard_project(args, roots, &masked, &mut made);
    held::hold_configuration_in_named_roots(args, roots, &masked, &mut made);
    for p in project_secrets
        .iter()
        .filter(|l| !masked.contains(&l.base))
        .flat_map(|l| l.existing())
    {
        hide(args, &p);
    }
    made
}

fn hide_fenced(args: &mut Vec<String>, roots: &Roots) {
    let state_roots = roots
        .state_root
        .iter()
        .chain(&roots.foreign_state_roots)
        .map(PathBuf::as_path);
    for socket in daemon_sockets(state_roots) {
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
            shut(args, p);
            for target in link_targets_outside(p) {
                shut(args, &target);
            }
        }
    }
    for p in &policy.deny_read_exact {
        shut(args, p);
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
    static AVAILABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        std::process::Command::new(bwrap_program())
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    })
}
