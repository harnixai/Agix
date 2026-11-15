//! Hook `if` pattern matching utilities.
//!
//! Supports:
//! - `""` or `*` — every call, and events that carry no tool at all
//! - `Bash` — exact tool name match
//! - `Bash(<content>)` — tool name + content (prefix:*, glob, exact); for
//!   `Bash` and `Monitor` the content is matched against what the command
//!   runs, as a deny rule matches it
//! - Other tool names (Read, Edit, mcp__*) — same rules, against the text
//! - `Edit|Write`, `Bash(git push:*)|Write` — any of the alternatives; a `|`
//!   inside parentheses belongs to the content, not the alternation

use crate::payload::HookInput;
use base::interface::tool_selector::ToolPattern;

/// Check whether a hook's `if` pattern matches the given input.
pub(crate) fn if_matches(pattern: &str, input: &HookInput) -> bool {
    let pat = pattern.trim();
    if pat.is_empty() || pat == "*" {
        return true;
    }
    alternatives(pat)
        .into_iter()
        .any(|alt| alternative_matches(alt.trim(), input))
}

fn alternatives(pattern: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut start = 0;
    for (i, c) in pattern.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            '|' if depth == 0 => {
                out.push(&pattern[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(&pattern[start..]);
    out
}

fn alternative_matches(pat: &str, input: &HookInput) -> bool {
    if pat == "*" {
        return true;
    }
    let Some(tool_name) = input.tool_name.as_deref() else {
        return false;
    };

    let (rule_tool, rule_content) = match pat.find('(') {
        Some(i) if pat.ends_with(')') => (
            pat[..i].trim().to_string(),
            Some(pat[i + 1..pat.len() - 1].to_string()),
        ),
        _ => (pat.to_string(), None),
    };

    if !match_tool_name(&rule_tool, tool_name) {
        return false;
    }

    let Some(rc) = rule_content else {
        return true;
    };
    let Some(input_value) = input.tool_input.as_ref() else {
        return false;
    };
    let field = |key: &str| input_value.get(key).and_then(|v| v.as_str());
    if let Some(command) = field("command") {
        if SHELL_TOOLS.contains(&tool_name) {
            return shell_matches(&rc, command);
        }
        return match_content(&rc, command);
    }
    if let Some(path) = PATH_FIELDS.iter().find_map(|key| field(key)) {
        return path_candidates(path, &input.cwd)
            .iter()
            .any(|candidate| match_content(&rc, candidate));
    }
    field("url").is_some_and(|url| match_content(&rc, url))
}

const PATH_FIELDS: &[&str] = &["file_path", "path", "notebook_path"];

/// The spellings a path argument can be matched as: the absolute path with
/// `.` and `..` folded away, where it lands on disk, and the same relative to
/// the session's cwd. A pattern written in any of those forms catches the call
/// however the model spelled the path.
fn path_candidates(raw: &str, cwd: &str) -> Vec<String> {
    use base::path::{normalize_lexically, resolve_physically};
    use std::path::Path;
    let joined = Path::new(cwd).join(raw);
    let lexical = normalize_lexically(&joined);
    let physical = resolve_physically(&joined);
    let mut out = vec![raw.to_string()];
    for abs in [&lexical, &physical] {
        out.push(abs.to_string_lossy().into_owned());
        if let Ok(rel) = abs.strip_prefix(normalize_lexically(Path::new(cwd))) {
            out.push(rel.to_string_lossy().into_owned());
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Tools whose content is a shell command line.
const SHELL_TOOLS: &[&str] = &["Bash", "Monitor"];

/// Whether `line` runs what `pattern` names, read the way a `Bash(...)` deny
/// rule reads it: the whole line, each simple command in it, and each
/// program it starts with quotes, wrappers and paths taken off. A command
/// whose program cannot be told from its text matches too, as it does a deny
/// rule — a hook that guards a command must not be talked past.
fn shell_matches(pattern: &str, line: &str) -> bool {
    use base::permission::{PermissionRule, RuleBehavior, RuleSource};
    use permissions::ruleset::{RuleHit, RuleSet};
    let mut rules = RuleSet::empty();
    rules.add(PermissionRule {
        source: RuleSource::UserSettings,
        behavior: RuleBehavior::Deny,
        tool_name: "Bash".into(),
        rule_content: Some(pattern.to_string()),
    });
    matches!(
        rules.evaluate_shell("Bash", None, line),
        RuleHit::Deny(_) | RuleHit::Unclear(_)
    )
}

fn match_tool_name(rule: &str, actual: &str) -> bool {
    if rule == actual {
        return true;
    }
    if let Some(prefix) = rule.strip_prefix("mcp__") {
        if let Some(after) = actual.strip_prefix("mcp__") {
            return after == prefix || after.starts_with(&format!("{prefix}__"));
        }
    }
    false
}

/// A tool selector pattern ([`ToolPattern`]) on the name alone: a hook payload names the tool, not
/// its origin, and an MCP tool's name is `mcp__<server>__<tool>`.
pub fn tool_matches(pattern: &str, name: &str) -> bool {
    match ToolPattern::parse(pattern) {
        ToolPattern::Name(n) => name == n,
        ToolPattern::Prefix(p) => name.starts_with(p.as_str()),
        ToolPattern::McpServer(s) => name.starts_with(&format!("mcp__{s}__")),
        ToolPattern::AnyMcp => name.starts_with("mcp__"),
    }
}

fn match_content(pattern: &str, content: &str) -> bool {
    if let Some(prefix) = pattern.strip_suffix(":*") {
        return content == prefix || content.starts_with(&format!("{prefix} "));
    }
    if pattern.contains('*') || pattern.contains('?') || pattern.contains('[') {
        if let Ok(g) = globset::Glob::new(pattern) {
            return g.compile_matcher().is_match(content);
        }
        return false;
    }
    pattern == content
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::payload::HookInput;
    use serde_json::json;

    fn make_input(tool_name: &str, command: Option<&str>) -> HookInput {
        HookInput {
            hook_event_name: "PreToolUse".into(),
            session_id: "test".into(),
            cwd: "/tmp".into(),
            permission_mode: "default".into(),
            tool_name: Some(tool_name.into()),
            tool_input: command.map(|c| json!({"command": c})),
            tool_use_id: Some("toolu_01".into()),
            tool_result: None,
            is_error: None,
            user_prompt: None,
            ..Default::default()
        }
    }

    #[test]
    fn match_tool_name_exact() {
        assert!(match_tool_name("Bash", "Bash"));
        assert!(!match_tool_name("Bash", "Read"));
    }

    #[test]
    fn match_tool_name_mcp_prefix() {
        assert!(match_tool_name("mcp__github", "mcp__github"));
        assert!(match_tool_name("mcp__github", "mcp__github__list_issues"));
        assert!(!match_tool_name("mcp__github", "Bash"));
        assert!(!match_tool_name("mcp__github", "mcp__slack"));
    }

    #[test]
    fn match_content_prefix_wildcard() {
        assert!(match_content("git push:*", "git push origin main"));
        assert!(match_content("git push:*", "git push"));
        assert!(!match_content("git push:*", "git pull"));
    }

    #[test]
    fn match_content_glob() {
        assert!(match_content("*.rs", "main.rs"));
        assert!(match_content("src/**/*.rs", "src/main.rs"));
        assert!(!match_content("*.rs", "main.py"));
    }

    #[test]
    fn match_content_exact() {
        assert!(match_content("ls", "ls"));
        assert!(!match_content("ls", "ls -la"));
    }

    #[test]
    fn if_matches_tool_name_only() {
        let input = make_input("Bash", Some("ls"));
        assert!(if_matches("Bash", &input));
        assert!(!if_matches("Read", &input));
    }

    #[test]
    fn if_matches_tool_name_with_content() {
        let input = make_input("Bash", Some("git push origin main"));
        assert!(if_matches("Bash(git push:*)", &input));
        assert!(!if_matches("Bash(ls)", &input));
    }

    #[test]
    fn if_matches_no_tool_name() {
        let input = HookInput {
            tool_name: None,
            ..make_input("Bash", Some("ls"))
        };
        assert!(!if_matches("Bash", &input));
    }

    #[test]
    fn if_matches_no_tool_input() {
        let input = make_input("Bash", None);
        assert!(if_matches("Bash", &input)); // tool name matches, no content filter needed
        assert!(!if_matches("Bash(ls)", &input)); // content filter fails without input
    }

    #[test]
    fn if_matches_mcp_tool() {
        let input = make_input("mcp__github__list_issues", None);
        assert!(if_matches("mcp__github", &input));
    }

    #[test]
    fn an_empty_or_star_pattern_matches_everything() {
        let bash = make_input("Bash", Some("ls"));
        let no_tool = HookInput {
            tool_name: None,
            ..make_input("Bash", None)
        };
        for pattern in ["", "  ", "*"] {
            assert!(if_matches(pattern, &bash), "{pattern:?}");
            assert!(
                if_matches(pattern, &no_tool),
                "{pattern:?} on a tool-less event"
            );
        }
    }

    #[test]
    fn alternatives_match_any_of_them() {
        let write = make_input("Write", None);
        let edit = make_input("Edit", None);
        let read = make_input("Read", None);
        assert!(if_matches("Edit|Write", &write));
        assert!(if_matches("Edit | Write", &edit));
        assert!(!if_matches("Edit|Write", &read));

        let push = make_input("Bash", Some("git push origin"));
        let ls = make_input("Bash", Some("ls"));
        assert!(if_matches("Bash(git push:*)|Write", &push));
        assert!(!if_matches("Bash(git push:*)|Write", &ls));
        assert!(if_matches("Bash(git push:*)|Write", &write));
    }

    /// A `Bash(...)` pattern is about the program that runs, as a deny rule
    /// is: a hook that stands guard over `rm` fires however the command
    /// spells it, and where the program cannot be told from the text.
    #[test]
    fn a_shell_pattern_matches_what_runs_however_it_is_spelled() {
        for command in [
            "rm -rf x",
            "cd . && rm -rf x",
            "ls; rm x",
            "\"rm\" x",
            "\\rm x",
            "/bin/rm x",
            "env rm x",
            "env -i rm x",
            "sudo rm -rf x",
            "timeout 5 rm x",
            "bash -c 'rm x'",
            "find . -exec rm {} +",
            "$RM x",
        ] {
            assert!(
                if_matches("Bash(rm:*)", &make_input("Bash", Some(command))),
                "`{command}`"
            );
        }
        for command in ["ls", "echo rm", "git rm x", "grep -r rm ."] {
            assert!(
                !if_matches("Bash(rm:*)", &make_input("Bash", Some(command))),
                "`{command}`"
            );
        }
        assert!(if_matches(
            "Monitor(rm:*)",
            &make_input("Monitor", Some("tail -f log; /bin/rm x"))
        ));
        assert!(if_matches(
            "Bash(git push:*)",
            &make_input("Bash", Some("cd repo && git push origin main"))
        ));
    }

    #[test]
    fn a_bar_inside_parentheses_is_content_not_an_alternative() {
        let piped = make_input("Bash", Some("ls | wc -l"));
        assert!(if_matches("Bash(ls | wc -l)", &piped));
        assert!(!if_matches(
            "Bash(ls | wc -l)",
            &make_input("Bash", Some("ls"))
        ));
        assert!(!if_matches("Bash(ls | wc -l)", &make_input("wc -l)", None)));
    }

    #[test]
    fn a_path_matches_however_it_is_spelled() {
        let write = |path: &str, key: &str| {
            let mut input = make_input("Write", None);
            input.cwd = "/work/proj".into();
            input.tool_input = Some(json!({ key: path }));
            input
        };
        for spelled in [
            "/work/proj/src/a.rs",
            "src/a.rs",
            "./src/a.rs",
            "src/../src/a.rs",
            "lib/../../proj/src/a.rs",
        ] {
            for pattern in ["Write(/work/proj/src/a.rs)", "Write(src/a.rs)"] {
                assert!(
                    if_matches(pattern, &write(spelled, "file_path")),
                    "{pattern} vs {spelled}"
                );
            }
        }
        assert!(!if_matches(
            "Write(/work/proj/src/a.rs)",
            &write("src/b.rs", "file_path")
        ));
        assert!(if_matches("NotebookEdit(/work/proj/n.ipynb)", &{
            let mut i = write("./n.ipynb", "notebook_path");
            i.tool_name = Some("NotebookEdit".into());
            i
        }));
    }
}
