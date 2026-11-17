//! The macOS backend: builds a `sandbox-exec` TinyScheme profile from a policy, one
//! rule group per concern (writes, state-root fence, project guard, reads, IPC,
//! network, daemon fences), last match winning.

use super::fence::{fenced_ports, fenced_secrets, fenced_sockets};
use super::roots::{link_targets_outside, project_guard, Roots};
use super::{
    egress, multiplexer_paths, patterns, read_scope, sandbox_escape, CommandEnv, Confined,
    Enforcement, NetworkMode, ProcessSpec, SandboxMode, SandboxPolicy,
};
use base::paths::STATE_ROOT_WORKING_DIRS;
use std::path::{Path, PathBuf};

pub(super) fn mac_wrap(
    inner: ProcessSpec,
    cwd: &Path,
    additional_writable: &[PathBuf],
    policy: &SandboxPolicy,
    env: CommandEnv<'_>,
) -> Confined {
    let profile = build_macos_profile(
        cwd,
        additional_writable,
        policy,
        &read_scope::UserDirs::of(env),
    );
    let mut unmet = domains_sandbox_exec_cannot_allow(policy);
    let mut spec = ProcessSpec::new("sandbox-exec", &inner.cwd)
        .arg("-p")
        .arg(profile)
        .arg(inner.program)
        .args(inner.args);
    if policy.network_mode == NetworkMode::Proxy {
        let proxy = egress::seatbelt(policy.egress_proxy.as_ref());
        unmet.extend(proxy.unmet);
        spec.env = proxy.env;
    }
    Confined {
        enforcement: if unmet.is_empty() {
            Enforcement::Full
        } else {
            Enforcement::Partial
        },
        unmet,
        spec,
        mode: SandboxMode::MacOSSandboxExec,
    }
}

pub(super) fn domains_sandbox_exec_cannot_allow(policy: &SandboxPolicy) -> Vec<String> {
    if policy.network_mode != NetworkMode::Allowlist {
        return Vec::new();
    }
    let unreachable: Vec<&str> = policy
        .allowed_domains
        .iter()
        .map(String::as_str)
        .filter(|d| !base::interface::exec::names_loopback(d))
        .collect();
    if unreachable.is_empty() {
        return Vec::new();
    }
    vec![format!(
        "domain allowlist: sandbox-exec can let through only localhost, so {} stays unreachable",
        unreachable.join(", ")
    )]
}

/// This user's `DARWIN_USER_TEMP_DIR`, resolved.
pub(super) fn darwin_user_temp_dir() -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    let mut buf = vec![0u8; libc::PATH_MAX as usize];
    let len = unsafe {
        libc::confstr(
            libc::_CS_DARWIN_USER_TEMP_DIR,
            buf.as_mut_ptr().cast(),
            buf.len(),
        )
    };
    if len == 0 || len > buf.len() {
        return None;
    }
    buf.truncate(len - 1);
    Some(base::path::resolve_physically(Path::new(
        std::ffi::OsStr::from_bytes(&buf),
    )))
}

pub(super) fn build_macos_profile(
    cwd: &Path,
    additional: &[PathBuf],
    policy: &SandboxPolicy,
    dirs: &read_scope::UserDirs,
) -> String {
    let roots = Roots::resolve(cwd, additional, policy);
    let mut s = String::with_capacity(4096);
    s.push_str("(version 1)\n");
    s.push_str("(allow default)\n");

    write_rules(&mut s, &roots);
    fence_state_root(&mut s, &roots);
    s.push_str(&patterns::deny_write_rules(&roots, &policy.deny_write));
    guard_project_files(&mut s, &roots);
    s.push_str(&patterns::scene_file_write_rules(&roots));

    read_rules(&mut s, policy, dirs);
    // After `allow_read`, which is for the user's own credential stores and
    // must not reopen the engine's.
    s.push_str(&patterns::credential_read_rules(&roots));

    ipc_rules(&mut s, policy);
    network_rules(&mut s, policy);
    daemon_fences(&mut s, &roots);
    s
}

fn quoted(p: &Path) -> String {
    sandbox_escape(&p.display().to_string())
}

fn write_rules(s: &mut String, roots: &Roots) {
    // sandbox-exec evaluates the profile top to bottom and the last matching
    // rule wins, so every deny below overrides the allows above it.
    s.push_str("(deny file-write*)\n");
    s.push_str("(allow file-write*\n");
    if !roots.only_additional {
        s.push_str(&format!("  (subpath \"{}\")\n", quoted(&roots.cwd)));
    }
    for p in super::implicit_writable_roots(roots.only_additional) {
        s.push_str(&format!("  (subpath \"{}\")\n", quoted(&p)));
    }
    for p in &roots.writable {
        s.push_str(&format!("  (subpath \"{}\")\n", quoted(p)));
    }
    s.push_str(")\n");
}

fn fence_state_root(s: &mut String, roots: &Roots) {
    for root in roots.fenced_foreign_state_roots() {
        s.push_str(&format!(
            "(deny file-write* (subpath \"{}\"))\n",
            quoted(root)
        ));
    }
    let Some(root) = roots.fenced_state_root() else {
        return;
    };
    s.push_str(&format!(
        "(deny file-write* (subpath \"{}\"))\n",
        quoted(root)
    ));
    s.push_str("(allow file-write*");
    for dir in STATE_ROOT_WORKING_DIRS {
        s.push_str(&format!(" (subpath \"{}\")", quoted(&root.join(dir))));
    }
    for dir in roots.session_temp_dirs_in(root) {
        s.push_str(&format!(" (subpath \"{}\")", quoted(dir)));
    }
    s.push_str(")\n");
}

fn guard_project_files(s: &mut String, roots: &Roots) {
    let guard = project_guard(roots);
    // A configuration root's working entries, where writable at all, and the
    // writable roots the policy names inside one are opened again before the
    // rules below, which then still hold the configuration inside them.
    for p in &guard.shut {
        s.push_str(&format!("(deny file-write* (subpath \"{}\"))\n", quoted(p)));
    }
    let reopened = guard.working.iter().filter(|p| roots.opens(p)).chain(
        roots
            .writable
            .iter()
            .filter(|w| guard.shut.iter().any(|root| w.starts_with(root))),
    );
    for p in reopened {
        s.push_str(&format!(
            "(allow file-write* (subpath \"{}\"))\n",
            quoted(p)
        ));
    }
    for p in guard.entries.iter().chain(&guard.files) {
        s.push_str(&format!("(deny file-write* (literal \"{}\"))\n", quoted(p)));
    }
    for p in &guard.trees {
        s.push_str(&format!("(deny file-write* (subpath \"{}\"))\n", quoted(p)));
    }
    // The engine's own git makes worktrees under such a policy.
    if !roots.only_additional {
        s.push_str(&patterns::worktree_control_file_rules(
            &guard.worktree_parents,
        ));
    }
    let (config_files, config_dirs) = roots.unfenced_state_root_config();
    for p in &config_files {
        s.push_str(&format!("(deny file-write* (literal \"{}\"))\n", quoted(p)));
    }
    for p in &config_dirs {
        s.push_str(&format!("(deny file-write* (subpath \"{}\"))\n", quoted(p)));
    }
}

fn read_rules(s: &mut String, policy: &SandboxPolicy, dirs: &read_scope::UserDirs) {
    s.push_str(&read_scope::macos_rules(&policy.read, dirs));
    for p in policy.deny_read.iter().flat_map(|p| spellings(p)) {
        s.push_str(&format!("(deny file-read* (subpath \"{}\"))\n", quoted(&p)));
    }
    for p in denied_link_targets(policy) {
        s.push_str(&format!("(deny file-read* (subpath \"{}\"))\n", quoted(&p)));
    }
    // Only an entry `deny_read` names is re-allowed, as on Linux: a parent
    // in `allow_read` written after the denies would win over every one of
    // them below it.
    let denied: Vec<PathBuf> = policy.deny_read.iter().flat_map(|p| spellings(p)).collect();
    for p in policy
        .allow_read
        .iter()
        .filter(|p| policy.read.names(p))
        .flat_map(|p| spellings(p))
        .filter(|p| denied.contains(p))
    {
        s.push_str(&format!(
            "(allow file-read* (subpath \"{}\"))\n",
            quoted(&p)
        ));
    }
    // After the re-allows, so that no `allow_read` spelling reopens them.
    for p in policy.deny_read_exact.iter().flat_map(|p| spellings(p)) {
        s.push_str(&format!("(deny file-read* (subpath \"{}\"))\n", quoted(&p)));
    }
}

fn denied_link_targets(policy: &SandboxPolicy) -> Vec<PathBuf> {
    let allowed: Vec<PathBuf> = policy
        .allow_read
        .iter()
        .flat_map(|p| spellings(p))
        .collect();
    policy
        .deny_read
        .iter()
        .flat_map(|p| spellings(p))
        .filter(|p| !allowed.contains(p))
        .flat_map(|p| link_targets_outside(&p))
        .collect()
}

fn ipc_rules(s: &mut String, policy: &SandboxPolicy) {
    // `launchctl submit` / `bootstrap` need no rule: launchd refuses a job
    // from a sandboxed caller.
    if !policy.deny_ipc_escapes {
        return;
    }
    s.push_str("(deny appleevent-send)\n");
    s.push_str("(deny lsopen)\n");
    s.push_str("(deny user-preference-write)\n");
    s.push_str("(deny mach-lookup (global-name \"com.apple.SecurityServer\"))\n");
    for p in multiplexer_paths() {
        s.push_str(&format!(
            "(deny file-read* file-write* (subpath \"{0}\"))\n\
             (deny network-outbound (remote unix-socket (subpath \"{0}\")))\n",
            quoted(&p)
        ));
    }
}

fn network_rules(s: &mut String, policy: &SandboxPolicy) {
    match policy.network_mode {
        NetworkMode::Unrestricted => {}
        NetworkMode::DenyAll => {
            s.push_str("(deny network*)\n");
        }
        NetworkMode::Allowlist => {
            s.push_str("(deny network*)\n");
            // A remote address names its host only as `*` or `localhost`; a
            // profile with `(remote tcp "example.com:*")` does not compile,
            // and every command under it fails. The other domains are
            // reported unmet instead.
            if policy
                .allowed_domains
                .iter()
                .any(|d| base::interface::exec::names_loopback(d))
            {
                s.push_str("(allow network-outbound (remote ip \"localhost:*\"))\n");
            }
        }
        NetworkMode::Proxy => {
            s.push_str(&egress::seatbelt(policy.egress_proxy.as_ref()).rules);
        }
    }
}

fn daemon_fences(s: &mut String, roots: &Roots) {
    // Last, so that no allowlist entry above reopens them. The kernel matches
    // a unix-socket connect against the socket's resolved path, so a symlink
    // or a relative name meets the same rule.
    let own = roots
        .state_root
        .as_deref()
        .filter(|root| !roots.project_in(root));
    for root in own
        .into_iter()
        .chain(roots.foreign_state_roots.iter().map(PathBuf::as_path))
    {
        s.push_str(&format!(
            "(deny network-outbound (remote unix-socket (subpath \"{}\")))\n",
            quoted(root)
        ));
    }
    for socket in fenced_sockets() {
        s.push_str(&format!(
            "(deny network-outbound (remote unix-socket (literal \"{}\")))\n",
            quoted(&socket)
        ));
    }
    for secret in fenced_secrets() {
        s.push_str(&format!(
            "(deny file-read* (literal \"{}\"))\n",
            quoted(&secret)
        ));
    }
    for addr in fenced_ports() {
        let host = if addr.ip().is_loopback() {
            "localhost"
        } else {
            "*"
        };
        s.push_str(&format!(
            "(deny network-outbound (remote tcp \"{host}:{}\"))\n",
            addr.port()
        ));
    }
}

/// `path` as given and as the kernel will see it, once each.
pub(super) fn spellings(path: &Path) -> Vec<PathBuf> {
    let resolved = base::path::resolve_physically(path);
    if resolved == path {
        vec![resolved]
    } else {
        vec![path.to_path_buf(), resolved]
    }
}
