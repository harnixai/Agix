//! Sandbox policy for shell-running tools: the session's sandbox settings and the paths its `Read` deny rules name, narrowed to what this call's approval allows, and the confined command.

use base::tool::ToolContext;
use permissions::path_rule;
use platform::sandbox;
use std::path::{Path, PathBuf};

pub(super) fn sandbox_policy(ctx: &ToolContext) -> sandbox::SandboxPolicy {
    let mut policy = session_policy(ctx);
    policy.additional_writable = writable_dirs(
        &ctx.session.env(),
        ctx.config.sandbox.session_tmp_dir(&ctx.session_id),
    );
    policy
}

/// The session's sandbox settings, with what its `Read` deny rules name
/// outright denied as named, past `allow_read`'s reach.
fn session_policy(ctx: &ToolContext) -> sandbox::SandboxPolicy {
    let mut policy = ctx.config.sandbox.policy();
    policy.deny_read_exact = read_denied_by_rules(ctx);
    policy
}

/// The paths the session's `Read` deny rules name outright, in each spelling
/// the permission layer judges them by: from cwd, and in a worktree from the
/// project root too. Left out, since no command could run without reading
/// it: one that is or holds a path a command starts in or may write
/// ([`session_roots`]), or that is, holds or lies in a system directory
/// programs load from ([`sandbox::system_read_roots`]).
fn read_denied_by_rules(ctx: &ToolContext) -> Vec<PathBuf> {
    let Some(denials) = &ctx.path_denials else {
        return Vec::new();
    };
    let env = ctx.session.env();
    let rule_roots: Vec<&Path> = std::iter::once(ctx.cwd.as_path())
        .chain(env.refusal_rule_roots.iter().map(PathBuf::as_path))
        .collect();
    let session = session_roots(ctx, &rule_roots);
    let system = sandbox::system_read_roots();
    let mut seen = Vec::new();
    let mut denied = Vec::new();
    for path in rule_roots
        .iter()
        .flat_map(|root| denials.denied_read_paths(root, &[]))
    {
        if seen.contains(&path) {
            continue;
        }
        seen.push(path.clone());
        let needed = session
            .iter()
            .chain(&system)
            .find(|dir| within(dir, &path))
            .or_else(|| system.iter().find(|dir| within(&path, dir)));
        match needed {
            Some(dir) => tracing::warn!(
                denied = %path.display(),
                needed = %dir.display(),
                "a Read deny rule covers what every sandboxed command reads to start; \
                 the sandbox leaves it to the permission layer, or no command could run"
            ),
            None => denied.push(path),
        }
    }
    denied
}

fn within(path: &Path, root: &Path) -> bool {
    path_rule::is_within_root_lexical(path, root) || path_rule::is_within_root_resolved(path, root)
}

/// Where a session's sandboxed command starts and what it may write: its
/// working directory and the project's, its writable directories and
/// `TMPDIR`, the state root, and the backends' implicit writable roots.
fn session_roots(ctx: &ToolContext, rule_roots: &[&Path]) -> Vec<PathBuf> {
    let env = ctx.session.env();
    let tmp = ctx.config.sandbox.session_tmp_dir(&ctx.session_id);
    rule_roots
        .iter()
        .map(|root| root.to_path_buf())
        .chain([env.cwd.clone()])
        .chain(writable_dirs(&env, tmp))
        .chain(ctx.config.sandbox.paths.state_root().map(Path::to_path_buf))
        .chain(sandbox::implicit_writable_roots(false))
        .collect()
}

/// What a session's sandboxed commands may write besides its working
/// directory and [`platform::sandbox::implicit_writable_roots`]: its writable
/// directories, its package caches, and `session_tmp`, its `TMPDIR`.
pub fn writable_dirs(
    env: &base::context::SessionEnv,
    session_tmp: std::path::PathBuf,
) -> Vec<std::path::PathBuf> {
    env.additional_writable_dirs
        .iter()
        .cloned()
        .chain(cache_directories(env))
        .chain([session_tmp])
        .collect()
}

/// The policy the call's approval says its command runs under. A call
/// nothing judged runs under the session's; a profile this does not know,
/// under the strictest.
pub(super) fn policy_for_profile(ctx: &ToolContext) -> sandbox::SandboxPolicy {
    use base::interface::permission::ExecProfile;
    match ctx.approval.as_ref().map(|a| a.profile) {
        None | Some(ExecProfile::Session) => sandbox_policy(ctx),
        Some(ExecProfile::Unattended) => {
            let mut policy = sandbox_policy(ctx);
            policy.deny_ipc_escapes = true;
            policy
        }
        Some(ExecProfile::Strict) => strict_sandbox_policy(ctx),
        Some(_) => strict_sandbox_policy(ctx),
    }
}

/// The policy a command let through on its own word runs under
/// (`ExecProfile::Strict`): it reads the session's readable roots and its
/// cache directories, writes only the session's `TMPDIR`.
pub fn strict_sandbox_policy(ctx: &ToolContext) -> sandbox::SandboxPolicy {
    let readable = crate::security::readable_roots(ctx)
        .into_iter()
        .chain(cache_directories(&ctx.session.env()));
    session_policy(ctx).strict(
        readable,
        Some(ctx.config.sandbox.session_tmp_dir(&ctx.session_id)),
    )
}

fn cache_directories(env: &base::context::SessionEnv) -> Vec<std::path::PathBuf> {
    let home = base::paths::home_dir();
    env.cache_directories
        .iter()
        .filter_map(|dir| match dir.strip_prefix("~") {
            Ok(rest) => home.as_ref().map(|h| h.join(rest)),
            Err(_) => Some(dir.clone()),
        })
        .collect()
}

/// `command` as the process a shell-running tool spawns: wrapped by the OS
/// sandbox under the profile its approval names, unless the session turned
/// the sandbox off. Whether a sandbox that cannot hold all of it may run it
/// at all is the permission policy's to say (`Stage::Confinement`). A
/// confined command's `TMPDIR` is the session's, once the runtime has made it.
pub(crate) fn confine_shell(
    tool: &str,
    command: &str,
    ctx: &ToolContext,
) -> base::interface::exec::ProcessSpec {
    use base::interface::exec::{Enforcement, ProcessSpec, SandboxMode};
    let intent = ProcessSpec::new("bash", &ctx.cwd).args(["-c".to_string(), command.to_string()]);
    if ctx.config.sandbox.disabled {
        return intent;
    }
    let mut confined = ctx.exec.sandbox.confine(intent, &policy_for_profile(ctx));
    if confined.enforcement != Enforcement::None {
        if let Some(tmp) = ctx.engine_git.tmpdir() {
            confined
                .spec
                .env
                .push(("TMPDIR".to_string(), tmp.display().to_string()));
        }
    }

    let short = match confined.enforcement {
        Enforcement::Full => false,
        Enforcement::None => confined.mode != SandboxMode::Disabled,
        _ => true,
    };
    if short {
        tracing::warn!(
            tool,
            platform = std::env::consts::OS,
            unmet = %confined.unmet.join("; "),
            "sandbox policy is not fully enforced. Install bwrap (Linux), \
             set sandbox.require_enforcement to refuse instead, or \
             dangerously_disable_sandbox to stop asking."
        );
    }
    confined.spec
}
