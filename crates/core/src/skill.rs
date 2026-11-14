//! Skill data types: what a loaded SKILL.md is ([`SkillEntry`]), where it came
//! from, and the `$ARGUMENTS`-style substitution applied to its body.

use std::path::PathBuf;

/// 单个 Skill：从 `<dir>/<skill_name>/SKILL.md` 的 YAML frontmatter 抽出。
#[derive(Debug, Clone)]
pub struct SkillEntry {
    /// 目录名（如 `summarize-pr`）；当 frontmatter 给 `name` 时取 frontmatter 值
    pub name: String,
    /// frontmatter `description`；不存在则取 markdown 第一行
    pub description: String,
    /// frontmatter `when_to_use`；可选
    pub when_to_use: Option<String>,
    pub source: SkillSource,
    /// SKILL.md 的物理路径（用于 `/skills` 命令展示）
    pub path: PathBuf,
    /// Hint shown to user for arguments (e.g. "commit message")
    pub argument_hint: Option<String>,
    /// Restrict tools this skill can invoke (whitelist of tool names)
    pub allowed_tools: Option<Vec<String>>,
    /// Tools removed from the pool while this skill is active (denylist,
    /// applied before `allowed_tools`). Clears on the next user message,
    /// same lifecycle as `allowed_tools`.
    pub disallowed_tools: Option<Vec<String>>,
    /// Named positional arguments (`arguments: [issue, branch]`) for `$name`
    /// substitution in the skill body, in addition to the existing
    /// positional `$1`..`$9`/`$ARGUMENTS` forms.
    pub arguments: Option<Vec<String>>,
    /// Override model for this skill
    pub model: Option<String>,
    /// Effort/thinking-mode override while this skill is active. Maps onto
    /// `base::interface::model::ThinkingMode` — Agix has no separate
    /// "effort" axis, so this is not a new concept, just this field's name.
    pub effort: Option<String>,
    /// Execution context: "fork" (sub-agent) or "inline" (default)
    pub context: Option<String>,
    /// Which agent type to run as when `context: "fork"` is set. Defaults to
    /// `general-purpose` when unset.
    pub agent: Option<String>,
    /// Only meaningful with `context: "fork"`. `false` waits for the forked
    /// result in the invoking turn instead of running it in the background.
    /// Default: `true` (background).
    pub background: Option<bool>,
    /// If true, model cannot invoke this skill directly (only user via slash)
    pub disable_model_invocation: bool,
    /// If false, skill is hidden from user-facing slash command list
    pub user_invocable: bool,
    /// Glob patterns for conditional activation (auto-load when matching files are touched)
    pub paths: Option<Vec<String>>,
    /// Version string for the skill
    pub version: Option<String>,
    /// Reference files bundled with the skill. Each entry is a path
    /// relative to the skill directory. When the skill is invoked, these files
    /// are read and their content is injected into the skill context.
    pub files: Option<Vec<String>>,
    /// Hook event names this skill subscribes to. When the skill is
    /// loaded, these hooks are registered with the HookRunner.
    pub hooks: Option<Vec<String>>,
}

impl Default for SkillEntry {
    fn default() -> Self {
        Self {
            name: String::new(),
            description: String::new(),
            when_to_use: None,
            source: SkillSource::User,
            path: PathBuf::new(),
            argument_hint: None,
            allowed_tools: None,
            disallowed_tools: None,
            arguments: None,
            model: None,
            effort: None,
            context: None,
            agent: None,
            background: None,
            disable_model_invocation: false,
            user_invocable: true,
            paths: None,
            version: None,
            files: None,
            hooks: None,
        }
    }
}

impl SkillEntry {
    /// Resolves `self.files` (paths relative to the skill's folder) to
    /// absolute paths. `self.path` is the SKILL.md file itself, so the
    /// skill folder is `self.path.parent()`. Returns an empty `Vec` if
    /// `files` is `None`/empty.
    ///
    /// Purely mechanical path joining — no existence checks or
    /// traversal/symlink-escape validation happen here. Callers that need
    /// those (e.g. `SkillTool`, which must surface a visible error for any
    /// declared file that escapes the skill folder) run them separately via
    /// `permissions::path_safety`, which this `core` crate deliberately does
    /// not depend on.
    pub fn resolve_bundled_files(&self) -> Vec<PathBuf> {
        let Some(files) = &self.files else {
            return Vec::new();
        };
        let Some(skill_dir) = self.path.parent() else {
            return Vec::new();
        };
        files.iter().map(|f| skill_dir.join(f)).collect()
    }
}

/// Skill 来源 -- user / project；同名时后入者覆盖先入者（见下方扫描顺序）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillSource {
    /// `~/.agix/skills/`（全局默认）或 `~/.agix/scenes/<scope>/skills/`（scene 覆盖）
    User,
    /// `<cwd>/.agents/skills/`（外部事实标准，Codex 也扫描这个路径）
    Project,
    /// Loaded from a plugin's manifest (`~/.agix/plugins/<name>/SKILL.md` or
    /// `~/.agix/scenes/<scope>/plugins/<name>/SKILL.md`)
    Plugin,
}

impl SkillSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            SkillSource::User => "user",
            SkillSource::Project => "project",
            SkillSource::Plugin => "plugin",
        }
    }
}

/// Whether a skill body holds a command its expansion would run: an inline
/// `` !`cmd` `` or a ` ```! ` block.
pub fn body_runs_commands(body: &str) -> bool {
    body.contains("!`") || body.contains("```!")
}

/// What `${AGIX_SKILL_DIR}` and `${AGIX_SESSION_ID}` stand for in one
/// invocation.
pub struct InvocationVars<'a> {
    /// The skill's `SKILL.md`; its folder is `${AGIX_SKILL_DIR}`.
    pub skill_md: &'a std::path::Path,
    pub session_id: Option<&'a str>,
}

/// Expands the variables in a skill body: for each name in `arg_names`
/// (the skill's `arguments:` frontmatter list, in
/// order), `$name` in the body is replaced with the positional argument at
/// that index (whitespace-separated, same splitting as `$1`..`$9`). A name
/// with no corresponding argument expands to an empty string, unlike an
/// *indexed* placeholder with no argument, which is left unchanged — that
/// asymmetry is intentional, not a bug.
///
/// `${AGIX_SKILL_DIR}` and `${AGIX_SESSION_ID}` are replaced from `vars`;
/// one `vars` does not know — a skill with no folder of its own, a caller
/// with no session — is left as written.
pub fn expand_skill_vars_named(
    body: &str,
    args: &str,
    arg_names: &[String],
    vars: Option<&InvocationVars<'_>>,
) -> String {
    let positions: Vec<&str> = args.split_whitespace().collect();
    let mut out = body.to_string();
    let mut substituted = false;

    // Named args: $name -> positional value by declared order.
    for (i, name) in arg_names.iter().enumerate() {
        let placeholder = format!("${name}");
        if out.contains(&placeholder) {
            let value = positions.get(i).copied().unwrap_or("");
            out = out.replace(&placeholder, value);
            substituted = true;
        }
    }

    if out.contains("$ARGUMENTS") {
        out = out.replace("$ARGUMENTS", args);
        substituted = true;
    }
    // $@ -- all arguments
    if out.contains("$@") {
        out = out.replace("$@", args);
        substituted = true;
    }
    // Positional args $1..$9 (reverse to avoid $10 partial match)
    for i in (1..=9).rev() {
        let placeholder = format!("${i}");
        if out.contains(&placeholder) {
            let value = positions.get(i - 1).copied().unwrap_or("");
            out = out.replace(&placeholder, value);
            substituted = true;
        }
    }

    let skill_dir = vars
        .and_then(|v| v.skill_md.parent())
        .filter(|dir| !dir.as_os_str().is_empty());
    if let Some(dir) = skill_dir.filter(|_| out.contains("${AGIX_SKILL_DIR}")) {
        out = out.replace("${AGIX_SKILL_DIR}", &dir.display().to_string());
        substituted = true;
    }
    if let Some(sid) = vars
        .and_then(|v| v.session_id)
        .filter(|_| out.contains("${AGIX_SESSION_ID}"))
    {
        out = out.replace("${AGIX_SESSION_ID}", sid);
        substituted = true;
    }

    if !substituted && !args.is_empty() {
        format!("{out}\n\nUser arguments: {args}")
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_bundled_files_joins_relative_to_skill_dir() {
        let skill = SkillEntry {
            name: "foo".into(),
            path: PathBuf::from("/skills/foo/SKILL.md"),
            files: Some(vec!["notes.md".into(), "ref/extra.md".into()]),
            ..Default::default()
        };
        let resolved = skill.resolve_bundled_files();
        assert_eq!(
            resolved,
            vec![
                PathBuf::from("/skills/foo/notes.md"),
                PathBuf::from("/skills/foo/ref/extra.md"),
            ]
        );
    }

    #[test]
    fn resolve_bundled_files_empty_when_files_is_none() {
        let skill = SkillEntry {
            name: "foo".into(),
            path: PathBuf::from("/skills/foo/SKILL.md"),
            files: None,
            ..Default::default()
        };
        assert!(skill.resolve_bundled_files().is_empty());
    }

    #[test]
    fn resolve_bundled_files_empty_when_files_is_empty_vec() {
        let skill = SkillEntry {
            name: "foo".into(),
            path: PathBuf::from("/skills/foo/SKILL.md"),
            files: Some(vec![]),
            ..Default::default()
        };
        assert!(skill.resolve_bundled_files().is_empty());
    }

    #[test]
    fn resolve_bundled_files_does_not_reject_traversal_paths() {
        // Purely mechanical joining — traversal rejection is the caller's
        // job (SkillTool, via permissions::path_safety), not this method's.
        let skill = SkillEntry {
            name: "foo".into(),
            path: PathBuf::from("/skills/foo/SKILL.md"),
            files: Some(vec!["../../outside.md".into()]),
            ..Default::default()
        };
        assert_eq!(
            skill.resolve_bundled_files(),
            vec![PathBuf::from("/skills/foo/../../outside.md")]
        );
    }

    #[test]
    fn expand_named_args_substitute_by_declared_order() {
        let body = "Migrate $component from $from to $to.";
        let names = vec![
            "component".to_string(),
            "from".to_string(),
            "to".to_string(),
        ];
        let r = expand_skill_vars_named(body, "SearchBar React Vue", &names, None);
        assert_eq!(r, "Migrate SearchBar from React to Vue.");
    }

    #[test]
    fn expand_named_arg_with_no_matching_positional_becomes_empty() {
        let body = "value=$only";
        let names = vec!["only".to_string()];
        let r = expand_skill_vars_named(body, "", &names, None);
        assert_eq!(r, "value=");
    }

    #[test]
    fn empty_names_leave_only_the_positional_forms() {
        let body = "Run with $ARGUMENTS.";
        assert_eq!(
            expand_skill_vars_named(body, "x y z", &[], None),
            "Run with x y z."
        );
    }
}
