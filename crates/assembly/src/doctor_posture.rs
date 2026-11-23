//! Doctor checks that report the daemon's safety posture: settings and build
//! facts that are legitimate but that an operator should have seen written
//! down — a permission mode that never asks, hooks that fail open, unattended
//! commands with an open network, a network allowlist the OS sandbox cannot
//! honour, an egress proxy it holds only in part, a strict profile the
//! sandbox cannot hold, a build whose panics end the process.

use base::interface::exec::NetworkMode;
use base::interface::exec::{ConfinementReport, Enforcement, ProcessSpec, Sandbox, SandboxPolicy};
use base::interface::health::{CheckResult, HealthCheck};
use base::permission::PermissionMode;
use hooks::{HookConfig, HookEvent, OnError};
use settings::LoadedSettings;
use std::sync::Arc;

pub struct PermissionModeCheck(Arc<LoadedSettings>);

impl PermissionModeCheck {
    pub fn new(settings: Arc<LoadedSettings>) -> Self {
        Self(settings)
    }
}

impl HealthCheck for PermissionModeCheck {
    fn name(&self) -> &str {
        "settings.permissions.mode"
    }

    fn check(&self) -> CheckResult {
        let mode = self.0.values.permissions.mode;
        let details = serde_json::json!({ "mode": mode });
        if mode == PermissionMode::BypassPermissions {
            CheckResult::degraded(
                "permissions.mode is bypassPermissions: tool calls run without asking",
            )
            .with_details(details)
        } else {
            CheckResult::ok("permission mode asks or restricts").with_details(details)
        }
    }
}

const INTERCEPTING_EVENTS: [HookEvent; 3] = [
    HookEvent::PreToolUse,
    HookEvent::UserPromptSubmit,
    HookEvent::Stop,
];

pub struct HooksFailOpenCheck(Arc<LoadedSettings>);

impl HooksFailOpenCheck {
    pub fn new(settings: Arc<LoadedSettings>) -> Self {
        Self(settings)
    }
}

fn on_error_of(config: &HookConfig) -> OnError {
    match config {
        HookConfig::Command { on_error, .. }
        | HookConfig::Prompt { on_error, .. }
        | HookConfig::Http { on_error, .. }
        | HookConfig::Agent { on_error, .. } => *on_error,
    }
}

impl HealthCheck for HooksFailOpenCheck {
    fn name(&self) -> &str {
        "settings.hooks_fail_open"
    }

    fn check(&self) -> CheckResult {
        let Some(value) = &self.0.values.hooks else {
            return CheckResult::ok("no hooks configured");
        };
        let (parsed, _) = hooks::parse_hooks_settings(value);
        let mut open: Vec<String> = INTERCEPTING_EVENTS
            .iter()
            .filter_map(|event| {
                let count = parsed
                    .get(event)?
                    .iter()
                    .filter(|h| on_error_of(h) == OnError::Allow)
                    .count();
                (count > 0).then(|| format!("{event:?} ({count})"))
            })
            .collect();
        open.sort();
        if open.is_empty() {
            return CheckResult::ok("no intercepting hook fails open");
        }
        CheckResult::degraded(format!(
            "hooks on {} let the action through when they fail (timeout, crash, bad output); \
             set \"on_error\": \"block\" to make a failure block",
            open.join(", ")
        ))
        .with_details(serde_json::json!({ "fail_open": open }))
    }
}

/// Unattended sessions auto-run a `Bash` / `Monitor` command the sandbox
/// fully holds; with the network open and the network policy not required,
/// such a command can still send what it reads anywhere.
pub struct UnattendedNetworkCheck {
    settings: Arc<LoadedSettings>,
    unattended_scenes: Vec<String>,
}

impl UnattendedNetworkCheck {
    pub fn new(settings: Arc<LoadedSettings>, unattended_scenes: Vec<String>) -> Self {
        Self {
            settings,
            unattended_scenes,
        }
    }
}

impl HealthCheck for UnattendedNetworkCheck {
    fn name(&self) -> &str {
        "sandbox.unattended_network"
    }

    fn check(&self) -> CheckResult {
        let sandbox = &self.settings.values.sandbox;
        let details = serde_json::json!({
            "unattended_scenes": self.unattended_scenes,
            "network_mode": sandbox.network.mode,
            "unattended_needs_network_policy": sandbox.unattended_needs_network_policy,
        });
        let open = sandbox.network.mode == NetworkMode::Unrestricted
            && !sandbox.unattended_needs_network_policy;
        if self.unattended_scenes.is_empty() {
            CheckResult::ok("no unattended scene is served")
        } else if !open {
            CheckResult::ok("unattended commands run without asking only under a network policy")
        } else {
            CheckResult::degraded(format!(
                "unattended scenes ({}) run sandboxed Bash / Monitor commands without asking \
                 while sandbox.network.mode is unrestricted, because \
                 sandbox.unattended_needs_network_policy is set to false: such a command can \
                 send what it reads anywhere. Remove that setting, or set sandbox.network.mode \
                 to deny_all / allowlist",
                self.unattended_scenes.join(", ")
            ))
        }
        .with_details(details)
    }
}

/// How the engine's own HTTP is routed: through the environment's proxy
/// (`system`) or not (`direct`). Names the proxy variables that are set,
/// never their values.
pub struct NetworkProxyCheck {
    use_system_proxy: bool,
    variables: Vec<String>,
}

impl NetworkProxyCheck {
    pub fn new(use_system_proxy: bool, variables: Vec<String>) -> Self {
        Self {
            use_system_proxy,
            variables,
        }
    }
}

impl HealthCheck for NetworkProxyCheck {
    fn name(&self) -> &str {
        "app.system_proxy"
    }

    fn check(&self) -> CheckResult {
        let (mode, summary) = if self.use_system_proxy {
            (
                "system",
                "the engine's HTTP follows the environment's proxy settings",
            )
        } else {
            (
                "direct",
                "app.system_proxy is false: the engine's HTTP connects directly",
            )
        };
        CheckResult::ok(summary).with_details(serde_json::json!({
            "mode": mode,
            "variables": self.variables,
        }))
    }
}

/// `sandbox.network.mode: allowlist` reads as "these domains and nothing else", and
/// for a sandboxed command it is "nothing else": neither OS backend filters
/// by domain.
pub struct NetworkAllowlistCheck(Arc<LoadedSettings>);

impl NetworkAllowlistCheck {
    pub fn new(settings: Arc<LoadedSettings>) -> Self {
        Self(settings)
    }
}

impl HealthCheck for NetworkAllowlistCheck {
    fn name(&self) -> &str {
        "sandbox.network_allowlist"
    }

    fn check(&self) -> CheckResult {
        let sandbox = &self.0.values.sandbox;
        if sandbox.network.mode != NetworkMode::Allowlist {
            return CheckResult::ok("sandbox.network.mode is not allowlist");
        }
        let unreachable: Vec<&str> = self
            .0
            .values
            .web
            .allowed_domains
            .iter()
            .map(String::as_str)
            .filter(|d| !base::interface::exec::names_loopback(d))
            .collect();
        let details = serde_json::json!({
            "unreachable_from_the_sandbox": unreachable,
            "require": sandbox.require,
        });
        if unreachable.is_empty() {
            return CheckResult::ok("the allowlist names only localhost").with_details(details);
        }
        let refused = if sandbox.require {
            "; with sandbox.require on, every sandboxed command is refused"
        } else {
            ""
        };
        CheckResult::degraded(format!(
            "sandbox.network.mode is allowlist, but the OS sandbox cannot filter by domain: a \
             sandboxed Bash / Monitor command reaches none of {} (macOS lets through only \
             localhost, and only when the allowlist names it; Linux cuts the network \
             entirely){refused}. WebFetch and Ping do follow the allowlist",
            unreachable.join(", ")
        ))
        .with_details(details)
    }
}

/// `sandbox.network.mode: proxy` is a promise both backends can keep, but not
/// everywhere: a proxy off this machine (macOS), no `socat` (Linux), no
/// proxy configured at all. What the session's sandbox makes of it is what
/// this reports.
pub struct EgressProxyCheck {
    settings: Arc<LoadedSettings>,
    sandbox: Arc<dyn Sandbox>,
}

impl EgressProxyCheck {
    pub fn new(settings: Arc<LoadedSettings>, sandbox: Arc<dyn Sandbox>) -> Self {
        Self { settings, sandbox }
    }
}

impl HealthCheck for EgressProxyCheck {
    fn name(&self) -> &str {
        "sandbox.network.proxy"
    }

    fn check(&self) -> CheckResult {
        let config = &self.settings.values.sandbox;
        let proxy = config.network.proxy.as_ref().map(|p| &p.0);
        let mut details = serde_json::json!({
            "network_mode": config.network.mode,
            "http": proxy.map(|p| p.http.to_string()),
            "socks": proxy.and_then(|p| p.socks).map(|s| s.to_string()),
        });
        if config.network.mode != NetworkMode::Proxy {
            return match proxy {
                Some(_) => CheckResult::degraded(
                    "sandbox.network.proxy is set but sandbox.network.mode is not proxy, so it \
                     is ignored",
                )
                .with_details(details),
                None => CheckResult::ok("sandbox.network.mode is not proxy"),
            };
        }
        if proxy.is_none() {
            return CheckResult::failing(
                "sandbox.network.mode is proxy but sandbox.network.proxy is missing or did not \
                 read: sandboxed commands run with the network cut",
            )
            .with_details(details);
        }
        if !config.enabled {
            return CheckResult::degraded(
                "sandbox.enabled is false: nothing holds commands to the proxy",
            )
            .with_details(details);
        }
        let policy = self.settings.engine_config().sandbox.policy();
        let probe = ProcessSpec::new("bash", std::env::temp_dir()).args(["-c", "true"]);
        let report = ConfinementReport::probe(&*self.sandbox, probe, &policy, false, false);
        details["enforcement"] = format!("{:?}", report.session).to_lowercase().into();
        details["unmet"] = report.session_unmet.clone().into();
        if report.session == Enforcement::Full {
            return CheckResult::ok("sandboxed commands reach only the egress proxy")
                .with_details(details);
        }
        CheckResult::degraded(format!(
            "the OS sandbox holds sandboxed commands to the egress proxy only in part: {}",
            report.session_unmet.join("; ")
        ))
        .with_details(details)
    }
}

/// A command let through without asking — the read-only lane — runs under the
/// strict profile: no network, nothing writable but the session's `TMPDIR`,
/// little readable. Where the sandbox cannot hold that profile the lane stays
/// open and the command runs without it.
pub struct StrictProfileCheck {
    settings: Arc<LoadedSettings>,
    sandbox: Arc<dyn Sandbox>,
}

impl StrictProfileCheck {
    pub fn new(settings: Arc<LoadedSettings>, sandbox: Arc<dyn Sandbox>) -> Self {
        Self { settings, sandbox }
    }
}

impl HealthCheck for StrictProfileCheck {
    fn name(&self) -> &str {
        "sandbox.strict_profile"
    }

    fn check(&self) -> CheckResult {
        let strict = if !self.settings.values.sandbox.enabled {
            Enforcement::None
        } else {
            let dir = std::env::temp_dir();
            let probe = ProcessSpec::new("bash", &dir).args(["-c", "true"]);
            ConfinementReport::probe(
                &*self.sandbox,
                probe,
                &SandboxPolicy::default(),
                false,
                false,
            )
            .strict
        };
        let details = serde_json::json!({
            "enforcement": format!("{strict:?}").to_lowercase(),
            "sandbox_enabled": self.settings.values.sandbox.enabled,
        });
        if strict == Enforcement::Full {
            return CheckResult::ok(
                "commands let through without asking run under the strict profile",
            )
            .with_details(details);
        }
        let why = if !self.settings.values.sandbox.enabled {
            "sandbox.enabled is false"
        } else {
            "this build or machine has no sandbox backend that holds it"
        };
        CheckResult::degraded(format!(
            "the strict profile is not enforced ({why}): read-only commands let through \
             without asking run without it, with the network, writes and reads the \
             session's own commands have"
        ))
        .with_details(details)
    }
}

pub struct PanicStrategyCheck;

impl PanicStrategyCheck {
    pub fn abort_build() -> bool {
        cfg!(panic = "abort")
    }
}

impl HealthCheck for PanicStrategyCheck {
    fn name(&self) -> &str {
        "build.panic"
    }

    fn check(&self) -> CheckResult {
        let abort = Self::abort_build();
        let details = serde_json::json!({ "panic": if abort { "abort" } else { "unwind" } });
        if abort {
            CheckResult::ok(
                "built with panic = \"abort\": a panic in a tool ends the process \
                 (let a supervisor restart it) instead of becoming that call's error",
            )
        } else {
            CheckResult::ok("built with panic = \"unwind\": a panic in a tool becomes its error")
        }
        .with_details(details)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use settings::Settings;

    fn settings_with(f: impl FnOnce(&mut Settings)) -> Arc<LoadedSettings> {
        let mut s = Settings::defaults_for("m");
        f(&mut s);
        Arc::new(LoadedSettings::new(s, base::paths::PathSettings::default()))
    }

    /// Holds the strict profile as `0` says, and every other policy fully.
    struct Holds(Enforcement);

    impl Sandbox for Holds {
        fn confine(
            &self,
            spec: ProcessSpec,
            policy: &SandboxPolicy,
        ) -> base::interface::exec::Confined {
            let strict = matches!(policy.read, base::interface::exec::ReadScope::Only(_));
            let enforcement = if strict { self.0 } else { Enforcement::Full };
            base::interface::exec::Confined {
                spec,
                mode: base::interface::exec::SandboxMode::MacOSSandboxExec,
                enforcement,
                unmet: Vec::new(),
            }
        }
    }

    struct HoldsProxy(Enforcement);

    impl Sandbox for HoldsProxy {
        fn confine(
            &self,
            spec: ProcessSpec,
            policy: &SandboxPolicy,
        ) -> base::interface::exec::Confined {
            let proxied =
                policy.network_mode == NetworkMode::Proxy && policy.egress_proxy.is_some();
            let enforcement = if proxied { self.0 } else { Enforcement::Full };
            base::interface::exec::Confined {
                spec,
                mode: base::interface::exec::SandboxMode::LinuxBwrap,
                enforcement,
                unmet: match enforcement {
                    Enforcement::Full => Vec::new(),
                    _ => vec!["egress proxy: socat not found".into()],
                },
            }
        }
    }

    #[test]
    fn the_egress_proxy_check_reports_what_the_sandbox_makes_of_it() {
        let check = |mode: NetworkMode, proxy: Option<&str>, held: Enforcement| {
            let settings = settings_with(|s| {
                s.sandbox.network.mode = mode;
                s.sandbox.network.proxy = proxy.map(|p| serde_json::from_value(p.into()).unwrap());
            });
            EgressProxyCheck::new(settings, Arc::new(HoldsProxy(held))).check()
        };
        let proxy = Some("127.0.0.1:3128");
        let unrestricted = check(NetworkMode::Unrestricted, None, Enforcement::Full);
        assert_eq!(unrestricted.status.label(), "ok");
        let missing = check(NetworkMode::Proxy, None, Enforcement::Full);
        assert_eq!(missing.status.label(), "failing", "{missing:?}");
        let held = check(NetworkMode::Proxy, proxy, Enforcement::Full);
        assert_eq!(held.status.label(), "ok", "{held:?}");
        let partial = check(NetworkMode::Proxy, proxy, Enforcement::Partial);
        assert_eq!(partial.status.label(), "degraded");
        assert!(partial.summary.contains("socat not found"), "{partial:?}");
        assert_eq!(partial.details["unmet"][0], "egress proxy: socat not found");
        let ignored = check(NetworkMode::DenyAll, proxy, Enforcement::Full);
        assert_eq!(ignored.status.label(), "degraded");
        assert!(ignored.summary.contains("ignored"), "{ignored:?}");
    }

    #[test]
    fn a_strict_profile_the_sandbox_cannot_hold_is_degraded() {
        let check = |settings, enforcement| {
            StrictProfileCheck::new(settings, Arc::new(Holds(enforcement))).check()
        };
        let held = check(settings_with(|_| {}), Enforcement::Full);
        assert_eq!(held.status.label(), "ok", "{held:?}");
        let unheld = check(settings_with(|_| {}), Enforcement::None);
        assert_eq!(unheld.status.label(), "degraded", "{unheld:?}");
        assert!(unheld.summary.contains("no sandbox backend"), "{unheld:?}");
        let off = check(
            settings_with(|s| s.sandbox.enabled = false),
            Enforcement::Full,
        );
        assert_eq!(off.status.label(), "degraded", "{off:?}");
    }

    #[test]
    fn only_all_three_together_flag_the_unattended_network() {
        let unattended = || vec!["code".to_string()];
        let status = |settings, scenes| {
            UnattendedNetworkCheck::new(settings, scenes)
                .check()
                .status
                .label()
        };
        let not_required = |s: &mut Settings| s.sandbox.unattended_needs_network_policy = false;
        assert_eq!(
            status(settings_with(not_required), unattended()),
            "degraded"
        );
        assert_eq!(status(settings_with(not_required), Vec::new()), "ok");
        assert_eq!(
            status(
                settings_with(|s| {
                    not_required(s);
                    s.sandbox.network.mode = NetworkMode::Allowlist;
                }),
                unattended()
            ),
            "ok"
        );
        assert_eq!(status(settings_with(|_| {}), unattended()), "ok");
    }

    #[test]
    fn the_proxy_check_reports_the_mode_and_only_variable_names() {
        let result = NetworkProxyCheck::new(true, vec!["HTTPS_PROXY".into()]).check();
        let details = result.details;
        assert_eq!(details["mode"], "system");
        assert_eq!(details["variables"], serde_json::json!(["HTTPS_PROXY"]));
        let direct = NetworkProxyCheck::new(false, Vec::new()).check();
        assert_eq!(direct.details["mode"], "direct");
    }

    #[test]
    fn an_allowlist_of_domains_is_flagged_as_unreachable_from_the_sandbox() {
        let allowlist = |domains: &[&str], require: bool| {
            NetworkAllowlistCheck::new(settings_with(|s| {
                s.sandbox.network.mode = NetworkMode::Allowlist;
                s.web.allowed_domains = domains.iter().map(|d| d.to_string()).collect();
                s.sandbox.require = require;
            }))
            .check()
        };
        let r = allowlist(&["registry.npmjs.org", "localhost"], false);
        assert_eq!(r.status.label(), "degraded", "{r:?}");
        assert!(r.summary.contains("registry.npmjs.org"), "{r:?}");
        assert!(!r.summary.contains("refused"), "{r:?}");
        assert_eq!(
            r.details["unreachable_from_the_sandbox"],
            serde_json::json!(["registry.npmjs.org"])
        );
        let r = allowlist(&["registry.npmjs.org"], true);
        assert!(
            r.summary.contains("every sandboxed command is refused"),
            "{r:?}"
        );
        assert_eq!(allowlist(&["localhost"], true).status.label(), "ok");
        assert_eq!(
            NetworkAllowlistCheck::new(settings_with(|s| {
                s.web.allowed_domains = vec!["registry.npmjs.org".into()];
            }))
            .check()
            .status
            .label(),
            "ok"
        );
    }

    #[test]
    fn bypass_permissions_is_flagged() {
        let ask = PermissionModeCheck::new(settings_with(|_| {})).check();
        assert!(ask.status.label() == "ok", "{ask:?}");
        let bypass = PermissionModeCheck::new(settings_with(|s| {
            s.permissions.mode = PermissionMode::BypassPermissions
        }))
        .check();
        assert_eq!(bypass.status.label(), "degraded");
        assert!(bypass.summary.contains("permissions.mode"));
    }

    #[test]
    fn a_pretooluse_hook_without_on_error_is_reported_and_a_blocking_one_is_not() {
        let open = settings_with(|s| {
            s.hooks = Some(serde_json::json!({
                "PreToolUse": [{ "type": "command", "command": "true" }],
                "PostToolUse": [{ "type": "command", "command": "true" }],
            }));
        });
        let r = HooksFailOpenCheck::new(open).check();
        assert_eq!(r.status.label(), "degraded", "{r:?}");
        assert!(r.summary.contains("PreToolUse"));
        assert!(!r.summary.contains("PostToolUse"));

        let closed = settings_with(|s| {
            s.hooks = Some(serde_json::json!({
                "PreToolUse": [{ "type": "command", "command": "true", "on_error": "block" }],
            }));
        });
        assert_eq!(HooksFailOpenCheck::new(closed).check().status.label(), "ok");
        assert_eq!(
            HooksFailOpenCheck::new(settings_with(|_| {}))
                .check()
                .status
                .label(),
            "ok"
        );
    }

    #[test]
    fn the_panic_strategy_is_reported_as_built() {
        let r = PanicStrategyCheck.check();
        let expected = if cfg!(panic = "abort") {
            "abort"
        } else {
            "unwind"
        };
        assert_eq!(r.details["panic"], expected);
    }
}
