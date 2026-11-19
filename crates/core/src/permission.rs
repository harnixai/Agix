//! 权限模式与权限规则。一次调用的判定结果是
//! `interface::permission::PermissionOutcome`。

use crate::interface::config_source::Tier;
use serde::{Deserialize, Serialize};

/// Permission mode for tool execution.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, schemars::JsonSchema,
)]
#[serde(rename_all = "camelCase")]
pub enum PermissionMode {
    /// Prompt user for each tool call that isn't explicitly allowed.
    #[default]
    Default,
    /// Allow file writes inside the project (the working directory and
    /// `files.extra_dirs`) and read-only calls without asking;
    /// ask about the rest, as `default` does. Writes into the project's
    /// `.git`, its project directory (`.agix` unless renamed; then `.agix`
    /// too), `.claude` and `.agents/skills` still ask.
    AcceptEdits,
    /// Ask nothing and skip the rules of the user's tiers. Credentials
    /// (`.ssh`, `.aws`, `.gnupg`, the state directory's settings and tokens)
    /// are still refused, the deployment's managed `deny` / `ask` rules still
    /// apply, input that is not valid is still refused, and `Bash` still runs
    /// in the OS sandbox.
    BypassPermissions,
    /// Plan mode — only allow read-only tools.
    Plan,
    /// Don't ask — deny any tool not explicitly allowed (no prompt).
    DontAsk,
    /// Bubble mode — forward permission requests up to a parent agent.
    /// **Program-only**: set by team/coordinator runtime; cannot be set by user.
    #[schemars(skip)]
    Bubble,
}

impl PermissionMode {
    /// Whether the user can set this mode via settings.json / CLI.
    /// `Bubble` is program-only — set by the team coordinator runtime and
    /// rejected from user config.
    pub fn is_user_settable(self) -> bool {
        !matches!(self, Self::Bubble)
    }

    /// How permissive this mode is, on a total order — higher grants more
    /// without asking.
    ///
    /// Exists so a host can *clamp* a mode it did not choose: the daemon's
    /// `session.create` RPC accepts a caller-supplied `permission_mode`, and
    /// without an ordering there was no way to express "a client may tighten
    /// this, but may not loosen it past what `settings.json` configured".
    /// Anyone who could reach the socket could simply ask for
    /// `bypassPermissions` and get it, whatever the daemon was configured
    /// with — see `daemon::session_pool::effective_permission_mode`.
    ///
    /// The ranking:
    /// - `DontAsk` refuses anything not explicitly allowed — nothing is more
    ///   restrictive.
    /// - `Plan` refuses every non-read-only tool.
    /// - `Default` asks about anything not explicitly allowed.
    /// - `Bubble` is `Default` with the question routed to a parent agent
    ///   rather than a human; the same set of calls still needs approval.
    /// - `AcceptEdits` additionally waves through `Write`/`Edit`.
    /// - `BypassPermissions` asks nothing at all.
    pub fn permissiveness(self) -> u8 {
        match self {
            Self::DontAsk => 0,
            Self::Plan => 1,
            Self::Default => 2,
            Self::Bubble => 3,
            Self::AcceptEdits => 4,
            Self::BypassPermissions => 5,
        }
    }

    /// The stricter (less permissive) of two modes. Ties keep `self`.
    pub fn min_permissive(self, other: Self) -> Self {
        if other.permissiveness() < self.permissiveness() {
            other
        } else {
            self
        }
    }

    /// The mode a delegate (sub-agent, team member) runs in when the session
    /// that spawned it is in `self`: its own, wider or narrower — except that a
    /// planning parent holds it to plan, a parent that asks nobody holds it to
    /// `dontAsk`, and a parent that bypasses takes it along, unless it may only
    /// tighten (`Delegate::mode_under`).
    pub fn for_delegate(self, delegate: Self) -> Self {
        match self {
            Self::Plan | Self::DontAsk => delegate.min_permissive(self),
            Self::BypassPermissions => self,
            _ => delegate,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
pub struct PermissionRule {
    pub source: RuleSource,
    pub behavior: RuleBehavior,
    pub tool_name: String,
    /// 例：Bash 用 "git push:*"；Read 用 "/etc/**"
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub rule_content: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum RuleSource {
    /// `~/.agix/scenes/<scope>/settings.json`
    UserSettings,
    /// `settings.json` in the project directory (`.agix` by default)
    ProjectSettings,
    /// `settings.local.json` in the project directory
    LocalSettings,
    /// CLI flag (`--permission-mode`, `--allow-tool` 等)
    CliArg,
    /// 会话内通过 `/permissions` 命令设置
    Session,
    /// 部署方的 managed settings（`settings::managed`）
    PolicySettings,
    /// 由命令展开时临时注入
    Command,
    /// 由已安装插件贡献。卸载插件时用 `Policy::remove_rules_by_source`
    /// 整批撤销。
    Plugin,
    /// 宿主自带的 `PermissionPolicy`：它的回答作为一层规则参与裁决。
    Host,
}

impl RuleSource {
    /// 同一行为有多条规则命中时，用它挑出报告哪一条：数值越大越先选中。
    /// 不参与 allow / ask / deny 之间的裁决——那只看行为，deny 永远优先。
    pub fn priority(self) -> u8 {
        match self {
            Self::PolicySettings => 70,
            Self::Host => 65,
            Self::CliArg => 60,
            Self::Session => 50,
            Self::Command => 45,
            Self::LocalSettings => 40,
            Self::ProjectSettings => 30,
            Self::UserSettings => 20,
            Self::Plugin => 5,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum RuleBehavior {
    Allow,
    Deny,
    Ask,
}

/// One rule string of a settings file's `permissions` block, with the list it
/// was written in: `allow`, `ask` and `deny` give the action, `allow_exact`
/// an allow whose content matches only the same text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionRuleEntry {
    /// As written: `Bash`, `Bash(git push:*)`, `mcp__srv__tool`.
    pub tool: String,
    pub action: RuleBehavior,
    pub matching: crate::interface::permission::GrantMatch,
}

/// A permission rule as loaded: the rule, the tier whose file it came from,
/// and whether that file was the tier's `settings.local.json`.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct LoadedRule {
    pub rule: PermissionRuleEntry,
    pub tier: Tier,
    pub machine_local: bool,
}

impl LoadedRule {
    pub fn new(rule: PermissionRuleEntry, tier: Tier, machine_local: bool) -> Self {
        Self {
            rule,
            tier,
            machine_local,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ManagedSettings {
    /// The file (or other source) the layer came from.
    pub origin: String,
    /// Why the layer cannot be honoured. While set, every tool call is refused.
    pub error: Option<String>,
    pub rules: Vec<PermissionRuleEntry>,
    pub disable_bypass: bool,
}

impl ManagedSettings {
    /// Two managed layers as one: both sets of rules, either one's switch,
    /// and broken if either is.
    pub fn followed_by(mut self, later: Self) -> Self {
        self.origin = format!("{}, {}", self.origin, later.origin);
        self.error = self.error.or(later.error);
        self.rules.extend(later.rules);
        self.disable_bypass |= later.disable_bypass;
        self
    }

    /// Whether a `bypassPermissions` session under this layer can still be
    /// asked about a call: the layer takes bypass away, or holds an `ask` or
    /// `deny` rule. A `deny` counts because a command it cannot read through
    /// (`$CMD x`) is asked about rather than refused.
    pub fn asks_under_bypass(&self) -> bool {
        self.error.is_none()
            && (self.disable_bypass || self.rules.iter().any(|r| r.action != RuleBehavior::Allow))
    }
}

/// What the permission engine reads of the loaded settings: every tier's
/// rules, the managed layer, and where a project-scoped grant is written. The
/// sandbox it reads off the call's `ToolContext::config`.
#[derive(Debug, Clone, Default)]
pub struct PermissionSettings {
    pub rules: Vec<LoadedRule>,
    pub managed: Option<ManagedSettings>,
    /// `None` for a session with no project: a grant lasts the session only.
    pub project_rules: Option<std::sync::Arc<dyn crate::interface::permission::RuleFile>>,
    /// Whose worktrees a path rule also matches in, as the main checkout
    /// holds them ([`crate::paths::Layout::main_checkout_copies`]).
    pub layout: crate::paths::Layout,
}

/// Whether the project tier of a `LoadedSettings` is in effect as written.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ProjectTrust {
    /// No project tier of its own: no project at all, or one whose `.agix`
    /// is the global or scene directory.
    #[default]
    NoProject,
    Trusted,
    Untrusted,
}

impl ProjectTrust {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NoProject => "none",
            Self::Trusted => "trusted",
            Self::Untrusted => "untrusted",
        }
    }

    pub fn is_untrusted(self) -> bool {
        self == Self::Untrusted
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rule_priority_ordering() {
        assert!(RuleSource::CliArg.priority() > RuleSource::LocalSettings.priority());
        assert!(RuleSource::LocalSettings.priority() > RuleSource::ProjectSettings.priority());
        assert!(RuleSource::ProjectSettings.priority() > RuleSource::UserSettings.priority());
        assert!(RuleSource::PolicySettings.priority() > RuleSource::CliArg.priority());
    }

    #[test]
    fn permission_mode_serializes_camelcase() {
        let s = serde_json::to_value(PermissionMode::BypassPermissions).unwrap();
        assert_eq!(s, serde_json::Value::String("bypassPermissions".into()));
        let back: PermissionMode = serde_json::from_value(s).unwrap();
        assert_eq!(back, PermissionMode::BypassPermissions);
    }

    #[test]
    fn only_the_claude_code_spellings_are_modes() {
        for spelling in ["auto", "yolo", "bypass_permissions"] {
            assert!(serde_json::from_value::<PermissionMode>(spelling.into()).is_err());
        }
    }

    #[test]
    fn rule_roundtrip() {
        let r = PermissionRule {
            source: RuleSource::ProjectSettings,
            behavior: RuleBehavior::Deny,
            tool_name: "Bash".into(),
            rule_content: Some("rm -rf *".into()),
        };
        let s = serde_json::to_string(&r).unwrap();
        let back: PermissionRule = serde_json::from_str(&s).unwrap();
        assert_eq!(r, back);
    }
}
