//! settings 文件的 `permissions` 块：权限模式、四组规则串（`allow` / `allow_exact` /
//! `ask` / `deny`）、提问超时与是否许客户端放宽。规则串不参与层间合并，每层逐条读成
//! `PermissionRuleEntry` 并列生效；managed 层也按这里读规则串。

use base::interface::permission::GrantMatch;
use base::permission::{PermissionMode, PermissionRuleEntry, RuleBehavior};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// The block's key in a settings file.
pub const KEY: &str = "permissions";

/// The rule lists, each with what a string in it is.
pub const RULE_LISTS: &[(&str, RuleBehavior, GrantMatch)] = &[
    ("allow", RuleBehavior::Allow, GrantMatch::Pattern),
    ("allow_exact", RuleBehavior::Allow, GrantMatch::Exact),
    ("ask", RuleBehavior::Ask, GrantMatch::Pattern),
    ("deny", RuleBehavior::Deny, GrantMatch::Pattern),
];

/// `permissions` in a settings file.
///
/// The rule lists are not merged like the rest: every tier's apply side by
/// side, a project's added to the global and scene ones, never substituted
/// for them; the merged settings hold none of them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct PermissionsSettings {
    /// Permission mode for tool execution.
    #[serde(default)]
    pub mode: PermissionMode,
    /// Rules that allow a call without asking: `Bash`, `Bash(git status:*)`,
    /// `Read(~/notes/**)`, `mcp__server__tool`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow: Vec<String>,
    /// Allow rules whose content is compared as written, not read as a
    /// pattern: `Bash(rm a*.tmp)` allows that command and no other. "Allow
    /// every time" records its answers here.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow_exact: Vec<String>,
    /// Rules that ask before a call, even one an allow rule covers.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ask: Vec<String>,
    /// Rules that refuse a call. A deny wins over any allow or ask.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deny: Vec<String>,
    /// How long a tool call waits for a host to answer a permission prompt
    /// before the call is denied. `0` waits indefinitely (only session
    /// cancellation interrupts it).
    ///
    /// The wait itself lives in the engine (`execute_tool_inner`), so the
    /// bound has to be reachable from `EngineConfig`, not only from a
    /// daemon-side flag — a host driving the engine directly needs it too.
    ///
    /// Failing closed (deny, not permit) on expiry is deliberate: a prompt
    /// that went unanswered is not consent.
    #[serde(default = "default_prompt_timeout_secs")]
    pub prompt_timeout_secs: u64,
    /// Whether an RPC client may ask for *looser* settings than these — a more
    /// permissive `mode` above, higher limits, allow rules.
    ///
    /// The daemon's `session.create` takes the session's own settings in
    /// `options.settings`. With this `false` — the default — a client can
    /// *tighten* them, but a looser value is clamped to what is configured
    /// here (`clamped_options`). Otherwise any client that could authenticate
    /// could open a `bypassPermissions` session no matter how the daemon was
    /// configured (`crates/assembly/src/server/mod.rs` notes there is no
    /// per-method authorization on that socket).
    ///
    /// Set it to `true` only when every client of this daemon is as trusted
    /// as the daemon itself.
    #[serde(default)]
    pub client_may_loosen: bool,
}

fn default_prompt_timeout_secs() -> u64 {
    300
}

impl Default for PermissionsSettings {
    fn default() -> Self {
        Self {
            mode: PermissionMode::default(),
            allow: Vec::new(),
            allow_exact: Vec::new(),
            ask: Vec::new(),
            deny: Vec::new(),
            prompt_timeout_secs: default_prompt_timeout_secs(),
            client_may_loosen: false,
        }
    }
}

/// `rules` as a `permissions` block holds them: each in its list, a list
/// with none left out.
pub fn rule_lists<'a>(
    rules: impl Iterator<Item = &'a PermissionRuleEntry> + Clone,
) -> Map<String, Value> {
    RULE_LISTS
        .iter()
        .filter_map(|(list, action, matching)| {
            let strings: Vec<Value> = rules
                .clone()
                .filter(|r| r.action == *action && r.matching == *matching)
                .map(|r| Value::String(r.tool.clone()))
                .collect();
            (!strings.is_empty()).then(|| (list.to_string(), Value::Array(strings)))
        })
        .collect()
}

/// Whether the dotted settings key is one of the rule lists, which the merged
/// values never hold.
pub fn names_a_rule_list(dotted: &str) -> bool {
    dotted
        .strip_prefix(KEY)
        .and_then(|rest| rest.strip_prefix('.'))
        .is_some_and(|list| RULE_LISTS.iter().any(|(name, _, _)| *name == list))
}

/// The rule lists of `block` — a `permissions` object — taken out of it, in
/// [`RULE_LISTS`] order. A list absent from it is absent here.
pub(crate) fn take_rule_lists(block: &mut Map<String, Value>) -> Vec<(&'static str, Value)> {
    RULE_LISTS
        .iter()
        .filter_map(|(list, _, _)| block.remove(*list).map(|value| (*list, value)))
        .collect()
}

/// One rule list, string by string: a string that is not a rule is left out
/// by itself (`permissions.allow[2]`, with why), so a mistyped allow does not
/// take the deny rules beside it along. A value that is not a list of
/// strings is left out as a whole.
pub(crate) fn read_rule_list(
    list: &str,
    value: Value,
) -> (Vec<PermissionRuleEntry>, Vec<(String, String)>) {
    let Some((_, action, matching)) = RULE_LISTS.iter().find(|(name, _, _)| *name == list) else {
        return (Vec::new(), Vec::new());
    };
    let Value::Array(entries) = value else {
        return (
            Vec::new(),
            vec![(
                format!("{KEY}.{list}"),
                format!("expected a list of rule strings, not {value}"),
            )],
        );
    };
    let mut kept = Vec::new();
    let mut unread = Vec::new();
    for (index, entry) in entries.into_iter().enumerate() {
        let at = format!("{KEY}.{list}[{index}]");
        let Value::String(tool) = entry else {
            unread.push((at, format!("expected a rule string, not {entry}")));
            continue;
        };
        match rule_text_problem(&tool) {
            None => kept.push(PermissionRuleEntry {
                tool,
                action: *action,
                matching: *matching,
            }),
            Some(problem) => unread.push((at, format!("{tool:?}: {problem}"))),
        }
    }
    (kept, unread)
}

/// What keeps a rule string such as `Bash(rm:*` from being a rule, in the
/// terms `permissions::rule::parse_rule_string` rejects it by.
fn rule_text_problem(tool: &str) -> Option<&'static str> {
    let tool = tool.trim();
    if tool.is_empty() {
        return Some("rule string is empty");
    }
    let open = tool.find('(')?;
    if !tool.ends_with(')') {
        return Some("rule has unbalanced parens");
    }
    tool[..open]
        .trim()
        .is_empty()
        .then_some("rule names no tool before the parens")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn each_list_gives_its_strings_their_action_and_match() {
        let read = |list: &str| {
            let (rules, _) = read_rule_list(list, json!(["Bash(rm a*.tmp)"]));
            let rule = &rules[0];
            (rule.tool.clone(), rule.action, rule.matching.is_pattern())
        };
        let tool = "Bash(rm a*.tmp)".to_string();
        assert_eq!(read("allow"), (tool.clone(), RuleBehavior::Allow, true));
        assert_eq!(
            read("allow_exact"),
            (tool.clone(), RuleBehavior::Allow, false)
        );
        assert_eq!(read("ask"), (tool.clone(), RuleBehavior::Ask, true));
        assert_eq!(read("deny"), (tool, RuleBehavior::Deny, true));
    }

    #[test]
    fn a_string_that_is_no_rule_is_left_out_by_itself() {
        let (kept, unread) = read_rule_list(
            "deny",
            json!(["Bash(rm:*)", "Bash(rm:*", 5, "Read(/etc/**)"]),
        );
        let kept: Vec<_> = kept.iter().map(|r| r.tool.as_str()).collect();
        assert_eq!(kept, ["Bash(rm:*)", "Read(/etc/**)"]);
        let at: Vec<_> = unread.iter().map(|(at, _)| at.as_str()).collect();
        assert_eq!(at, ["permissions.deny[1]", "permissions.deny[2]"]);
        assert!(unread[0].1.contains("unbalanced"), "{unread:?}");
    }

    #[test]
    fn a_list_that_is_not_a_list_is_left_out_whole() {
        let (kept, unread) = read_rule_list("allow", json!("Bash"));
        assert!(kept.is_empty());
        assert_eq!(unread[0].0, "permissions.allow");
    }
}
