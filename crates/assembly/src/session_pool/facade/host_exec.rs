//! 嵌入方的沙箱执行（`exec.run`）：宿主自己要跑的命令，放进与 `Bash` 同一套
//! `SandboxPolicy` 里跑。不经模型、不问权限、不进会话。
//!
//! 策略只放紧不放松：内建凭据名单加设置里的 `sandbox.deny_read` 加请求的 `deny_read`，
//! 任何一层的 `sandbox.allow_read` 都不读；managed 层照样生效——它读不了时什么都不跑，
//! 它的 `Bash(...)` deny 规则按 argv 拼成的命令行匹配、`Read(...)` deny 规则点名的路径
//! 读不到，它定了 `sandbox.network` 时 `network: true` 按它的来，它要求沙箱时
//! `allow_unconfined` 不起作用。
//!
//! 沙箱护着的项目配置（`.git` 的控制文件、引擎的配置目录）是按 `cwd` 算的，所以可写根
//! 只能在 `cwd` 里，或在 `cwd` 之外而根上不是项目。

use crate::session_pool::*;
use api::{
    HostExec, HostExecEnforcement, HostExecProfile, HostExecResult, HOST_EXEC_DEFAULT_OUTPUT,
    HOST_EXEC_DEFAULT_TIME, HOST_EXEC_OUTPUT_LIMIT, HOST_EXEC_TIME_LIMIT,
};
use base::interface::exec::{Enforcement, EnvScope, NetworkMode, ProcessSpec, SandboxPolicy};
use base::permission::{ManagedSettings, RuleBehavior, RuleSource};
use permissions::ruleset::{RuleHit, RuleSet};
use platform::drain_bounded;

impl SessionPool {
    /// Run `request` in the sandbox and hand back how it ended. `Err` is a
    /// command that was not started: a request that does not hold, or
    /// [`PoolError::ExecRefused`].
    pub async fn host_exec(
        &self,
        request: HostExec,
        cancel: CancellationToken,
    ) -> Result<HostExecResult, PoolError> {
        let time = time_of(&request)?;
        let limit = output_limit_of(&request)?;
        let (cwd, writable) = places_of(&request)?;
        check_env(&request)?;
        let settings = self.bindings.pool_settings().await;
        let managed = ManagedHold::of(settings.managed.as_ref())?;
        managed.admit(&request.argv)?;

        let mut values = settings.values.clone();
        values.sandbox.allow_read.clear();
        values
            .sandbox
            .deny_read
            .extend(request.deny_read.iter().cloned());
        let sandbox = settings::sandbox_settings(
            &values,
            &base::paths::ConfigPaths::from_settings(&settings.paths),
        );
        check_writable(&cwd, &writable, &sandbox.paths)?;
        let tmp = CallTmp::create(sandbox.session_tmp_dir(&format!("exec-{}", Id::new())))?;
        let mut policy = policy_of(&sandbox.policy(), &request, &managed, tmp.path(), &writable);
        policy.read_only_dirs = self.bindings.host_dirs().0.protected().get();
        policy.deny_read_exact = managed.read_denied(&cwd, &policy);
        let require = managed.requires_sandbox(&values);

        let exec = platform::local_providers();
        let confined = exec
            .sandbox
            .confine(spec_of(&request, &cwd, tmp.path()), &policy);
        if confined.enforcement == Enforcement::None && (require || !request.allow_unconfined) {
            return Err(refusal(&request.argv[0], &confined.unmet, require));
        }
        let started = Instant::now();
        let program = &request.argv[0];
        let child = exec
            .process
            .spawn(confined.spec, CancellationToken::new())
            .await
            .map_err(|e| PoolError::InvalidRequest(format!("could not start `{program}`: {e}")))?;
        let stop = async {
            tokio::select! {
                _ = tokio::time::sleep(time) => Stopped::TimedOut,
                _ = cancel.cancelled() => Stopped::Cancelled,
            }
        };
        let (captured, stopped) = drain_bounded(child, limit, stop).await.map_err(|e| {
            PoolError::Internal(format!("`{program}` started, but reading it failed: {e}"))
        })?;
        Ok(HostExecResult {
            code: captured.status.code,
            signal: captured.status.signal,
            stdout: String::from_utf8_lossy(&captured.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&captured.stderr).into_owned(),
            truncated: captured.truncated,
            timed_out: stopped == Some(Stopped::TimedOut),
            cancelled: stopped == Some(Stopped::Cancelled),
            ms: started.elapsed().as_millis() as u64,
            enforcement: match confined.enforcement {
                Enforcement::Full => HostExecEnforcement::Full,
                Enforcement::Partial => HostExecEnforcement::Partial,
                Enforcement::None => HostExecEnforcement::None,
            },
            unmet: confined.unmet,
        })
    }
}

/// The command as the request names it, in the minimal environment with
/// what the request adds and the call's own `TMPDIR`.
fn spec_of(request: &HostExec, cwd: &Path, tmp: &Path) -> ProcessSpec {
    let mut spec = ProcessSpec::new(request.argv[0].clone(), cwd).args(&request.argv[1..]);
    spec.env_scope = EnvScope::Minimal;
    spec.env = request.env.clone().into_iter().collect();
    spec.env
        .push(("TMPDIR".to_string(), tmp.display().to_string()));
    spec.stdin = request.stdin.clone();
    spec
}

fn refusal(program: &str, unmet: &[String], require: bool) -> PoolError {
    PoolError::ExecRefused(format!(
        "the sandbox cannot hold `{program}` here ({}){}",
        unmet.join("; "),
        if require {
            "; the managed settings require it (`sandbox.require`)"
        } else {
            "; pass `allow_unconfined` to run it anyway"
        }
    ))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stopped {
    TimedOut,
    Cancelled,
}

fn policy_of(
    base: &SandboxPolicy,
    request: &HostExec,
    managed: &ManagedHold,
    tmp: &Path,
    writable: &[PathBuf],
) -> SandboxPolicy {
    let mut policy = base.clone();
    policy.only_additional_writable = true;
    policy.additional_writable = std::iter::once(tmp.to_path_buf())
        .chain(writable.iter().cloned())
        .collect();
    policy.deny_ipc_escapes = true;
    policy.env = EnvScope::Minimal;
    if !request.network {
        policy.network_mode = NetworkMode::DenyAll;
        policy.egress_proxy = None;
        policy.allowed_domains.clear();
    } else if !managed.pins_network {
        policy.network_mode = NetworkMode::Unrestricted;
        policy.egress_proxy = None;
        policy.allowed_domains.clear();
    }
    policy
}

fn time_of(request: &HostExec) -> Result<Duration, PoolError> {
    match request.timeout {
        None => Ok(HOST_EXEC_DEFAULT_TIME),
        Some(t) if t.is_zero() => Err(PoolError::InvalidRequest(
            "`timeout_ms` is more than 0".into(),
        )),
        Some(t) if t > HOST_EXEC_TIME_LIMIT => Err(PoolError::InvalidRequest(format!(
            "a command runs for at most {} ms, not {} ms",
            HOST_EXEC_TIME_LIMIT.as_millis(),
            t.as_millis()
        ))),
        Some(t) => Ok(t),
    }
}

fn output_limit_of(request: &HostExec) -> Result<usize, PoolError> {
    match request.max_output_bytes {
        None => Ok(HOST_EXEC_DEFAULT_OUTPUT),
        Some(0) => Err(PoolError::InvalidRequest(
            "`max_output_bytes` is more than 0".into(),
        )),
        Some(n) if n > HOST_EXEC_OUTPUT_LIMIT => Err(PoolError::InvalidRequest(format!(
            "`max_output_bytes` is at most {HOST_EXEC_OUTPUT_LIMIT}, not {n}"
        ))),
        Some(n) => Ok(n),
    }
}

/// The working directory and the writable roots, as they are on disk.
fn places_of(request: &HostExec) -> Result<(PathBuf, Vec<PathBuf>), PoolError> {
    let invalid = |m: String| Err(PoolError::InvalidRequest(m));
    match request.argv.first() {
        None => return invalid("`argv` names the program to run".into()),
        Some(program) if program.is_empty() => return invalid("`argv[0]` is empty".into()),
        Some(_) => {}
    }
    if request.argv.iter().any(|a| a.contains('\0')) {
        return invalid("an `argv` entry holds a NUL byte".into());
    }
    if !request.cwd.is_absolute() || !request.cwd.is_dir() {
        return invalid(format!(
            "`cwd` is an absolute path to a directory: {}",
            request.cwd.display()
        ));
    }
    if request.profile == HostExecProfile::Read && !request.writable.is_empty() {
        return invalid("`writable` is for `profile: \"write\"`".into());
    }
    let mut writable = Vec::new();
    for path in &request.writable {
        if !path.is_absolute() {
            return invalid(format!(
                "`writable` holds absolute paths: {}",
                path.display()
            ));
        }
        match path.canonicalize() {
            Ok(real) => writable.push(real),
            Err(e) => return invalid(format!("`writable` {}: {e}", path.display())),
        }
    }
    if let Some(path) = request.deny_read.iter().find(|p| !p.is_absolute()) {
        return invalid(format!(
            "`deny_read` holds absolute paths: {}",
            path.display()
        ));
    }
    let cwd = request
        .cwd
        .canonicalize()
        .map_err(|e| PoolError::InvalidRequest(format!("`cwd`: {e}")))?;
    Ok((cwd, writable))
}

/// A writable root outside `cwd` that is itself a project would leave its
/// `.git` and engine configuration writable; one above `cwd` would leave
/// `cwd`'s project's.
fn check_writable(
    cwd: &Path,
    writable: &[PathBuf],
    paths: &base::paths::SensitivePaths,
) -> Result<(), PoolError> {
    for root in writable {
        if root.starts_with(cwd) {
            continue;
        }
        if cwd.starts_with(root) {
            return Err(PoolError::InvalidRequest(format!(
                "`writable` {} holds `cwd`; run the command there instead",
                root.display()
            )));
        }
        let project = std::iter::once(root.join(base::paths::REPOSITORY_CONFIG_DIR))
            .chain(paths.config_roots_in(root).into_iter().map(|r| r.path))
            .find(|p| p.symlink_metadata().is_ok());
        if let Some(found) = project {
            return Err(PoolError::InvalidRequest(format!(
                "`writable` {} is a project ({} is there); make it the `cwd` to write it",
                root.display(),
                found.display()
            )));
        }
    }
    Ok(())
}

/// The dynamic loader reads these before the sandbox's own wrapper has
/// confined anything, so code they name would run outside it.
const LOADER_VARIABLES: &[&str] = &["LD_", "DYLD_"];

/// What a shell reads from its environment: on Linux the wrapper outside the
/// sandbox is one.
const SHELL_VARIABLES: &[&str] = &["BASH_ENV", "ENV", "IFS", "SHELLOPTS", "BASHOPTS"];

fn check_env(request: &HostExec) -> Result<(), PoolError> {
    for (name, value) in &request.env {
        if name.is_empty() || name.contains(['=', '\0']) || value.contains('\0') {
            return Err(PoolError::InvalidRequest(format!(
                "`env` cannot hold the variable `{name}`"
            )));
        }
        if LOADER_VARIABLES.iter().any(|p| name.starts_with(p))
            || SHELL_VARIABLES.contains(&name.as_str())
            || name.starts_with("BASH_FUNC_")
        {
            return Err(PoolError::InvalidRequest(format!(
                "`env` cannot set `{name}`: it would reach the sandbox's own wrapper too"
            )));
        }
    }
    Ok(())
}

/// What of the managed layer holds a host command.
#[derive(Default)]
struct ManagedHold {
    denies: RuleSet,
    pins_network: bool,
    pins_require: bool,
}

impl ManagedHold {
    fn of(managed: Option<&ManagedSettings>) -> Result<Self, PoolError> {
        let Some(managed) = managed else {
            return Ok(Self::default());
        };
        if let Some(error) = &managed.error {
            return Err(PoolError::ExecRefused(format!(
                "the managed settings cannot be honoured ({error}), so no command runs"
            )));
        }
        let mut denies = RuleSet::empty();
        for entry in managed
            .rules
            .iter()
            .filter(|r| r.action == RuleBehavior::Deny)
        {
            match permissions::rule::parse_rule_string(
                &entry.tool,
                RuleSource::PolicySettings,
                entry.action,
            ) {
                Ok(rule) => denies.add(rule),
                Err(e) => {
                    return Err(PoolError::ExecRefused(format!(
                        "{}: permission rule `{}` does not parse ({e}), so no command runs",
                        managed.origin, entry.tool
                    )))
                }
            }
        }
        let sandbox = managed.values.get("sandbox");
        let pins = |key: &str| sandbox.and_then(|s| s.get(key)).is_some();
        Ok(Self {
            denies,
            pins_network: pins("network"),
            pins_require: pins("require"),
        })
    }

    fn admit(&self, argv: &[String]) -> Result<(), PoolError> {
        let line = command_line(argv);
        match self.denies.evaluate_shell("Bash", None, &line) {
            RuleHit::Deny(rule) => Err(PoolError::ExecRefused(format!(
                "the managed settings deny `{}`",
                permissions::rule::format_rule_string(&rule)
            ))),
            _ => Ok(()),
        }
    }

    /// What the layer's `Read(...)` deny rules name outright, but none that
    /// is or holds where the command starts or may write, or that a program
    /// loads from: as `Bash` leaves those to the permission layer, no command
    /// could start without them.
    fn read_denied(&self, cwd: &Path, policy: &SandboxPolicy) -> Vec<PathBuf> {
        let within = |path: &Path, root: &Path| {
            permissions::path_rule::is_within_root_lexical(path, root)
                || permissions::path_rule::is_within_root_resolved(path, root)
        };
        let needed: Vec<PathBuf> = std::iter::once(cwd.to_path_buf())
            .chain(policy.additional_writable.iter().cloned())
            .collect();
        let system = platform::sandbox::system_read_roots();
        permissions::ruleset::denied_read_paths(self.denies.rules(), cwd, &policy.layout)
            .into_iter()
            .filter(|path| {
                !needed.iter().chain(&system).any(|dir| within(dir, path))
                    && !system.iter().any(|dir| within(path, dir))
            })
            .collect()
    }

    fn requires_sandbox(&self, values: &Settings) -> bool {
        self.pins_require && values.sandbox.require
    }
}

/// `argv` as a shell would read it back, for the rules written against
/// command lines.
fn command_line(argv: &[String]) -> String {
    let plain = |a: &str| {
        !a.is_empty()
            && a.chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_./=:,+@%".contains(c))
    };
    argv.iter()
        .map(|a| match plain(a) {
            true => a.clone(),
            false => format!("'{}'", a.replace('\'', r"'\''")),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The call's own temporary directory, private to this user, gone with it.
struct CallTmp(PathBuf);

impl CallTmp {
    fn create(dir: PathBuf) -> Result<Self, PoolError> {
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        if let Some(parent) = dir.parent() {
            builder.recursive(true);
            let _ = builder.create(parent);
            builder.recursive(false);
        }
        builder.create(&dir).map_err(|e| {
            PoolError::Internal(format!(
                "could not create the command's TMPDIR {}: {e}",
                dir.display()
            ))
        })?;
        Ok(Self(dir))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for CallTmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_command_line_quotes_what_a_shell_would_split() {
        let argv = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            command_line(&argv(&["git", "log", "--format=%H"])),
            "git log --format=%H"
        );
        assert_eq!(
            command_line(&argv(&["sh", "-c", "curl x; it's"])),
            r"sh -c 'curl x; it'\''s'"
        );
        assert_eq!(command_line(&argv(&["echo", ""])), "echo ''");
    }

    fn managed(rules: &[&str], values: serde_json::Value) -> ManagedSettings {
        ManagedSettings {
            origin: "managed.json".into(),
            error: None,
            rules: rules
                .iter()
                .map(|tool| base::permission::PermissionRuleEntry {
                    tool: tool.to_string(),
                    action: RuleBehavior::Deny,
                    matching: Default::default(),
                })
                .collect(),
            disable_bypass: false,
            values: values.as_object().cloned().unwrap_or_default(),
        }
    }

    #[test]
    fn a_managed_deny_rule_refuses_the_command_it_names() {
        let hold =
            ManagedHold::of(Some(&managed(&["Bash(curl:*)"], serde_json::json!({})))).unwrap();
        let argv = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(hold.admit(&argv(&["curl", "https://x"])).is_err());
        assert!(hold.admit(&argv(&["/usr/bin/curl", "x"])).is_err());
        assert!(hold.admit(&argv(&["git", "status"])).is_ok());
        assert!(!hold.pins_network && !hold.pins_require);
    }

    #[test]
    fn an_unreadable_managed_layer_runs_nothing() {
        let mut layer = managed(&[], serde_json::json!({}));
        layer.error = Some("bad rule".into());
        assert!(matches!(
            ManagedHold::of(Some(&layer)),
            Err(PoolError::ExecRefused(_))
        ));
    }

    #[test]
    fn what_the_managed_layer_sets_of_the_sandbox_is_what_it_pins() {
        let hold = ManagedHold::of(Some(&managed(
            &[],
            serde_json::json!({"sandbox": {"require": true, "network": {"mode": "deny_all"}}}),
        )))
        .unwrap();
        assert!(hold.pins_network && hold.pins_require);
    }
}
