//! The markdown files the engine reads into the prompt: `SKILL.md`
//! frontmatter, `AGENTS.md` collected from the working directory upward, output
//! styles, the configured instruction file and the `.agix/rules/` directories.

mod agents_md;
mod output_style;

pub use agents_md::collect_agents_md;
pub use output_style::load_output_style;

use std::path::Path;

use base::skill::{SkillEntry, SkillSource};
use base::text::frontmatter::{first_paragraph, parse_yaml_list, split_frontmatter, strip_quotes};

/// The frontmatter keys [`parse_skill_file`] reads, aliases included. Kept
/// beside its `match` so the two change together: a checker that refuses
/// unknown keys reads this list.
pub const SKILL_FRONTMATTER_KEYS: &[&str] = &[
    "name",
    "description",
    "when_to_use",
    "whenToUse",
    "argument_hint",
    "argumentHint",
    "allowed_tools",
    "allowedTools",
    "allowed-tools",
    "disallowed_tools",
    "disallowedTools",
    "disallowed-tools",
    "arguments",
    "model",
    "effort",
    "context",
    "agent",
    "background",
    "disable_model_invocation",
    "disableModelInvocation",
    "user_invocable",
    "userInvocable",
    "paths",
    "version",
    "files",
    "hooks",
];

/// Parse SKILL.md: extract YAML frontmatter (`---` delimited `key: value` lines),
/// extracting name / description / when_to_use and extended fields.
///
/// Handles common YAML scalars: bare strings, quoted strings, and inline lists
/// (`[a, b, c]`). Multi-line values (`|`, `>`) and nested maps are NOT supported.
///
/// Frontmatter missing: description from first markdown line; name from dir name.
/// description still empty -> skill is considered invalid, return None.
pub fn parse_skill_file(
    content: &str,
    dir_name: String,
    path: &Path,
    source: SkillSource,
) -> Option<SkillEntry> {
    let (front, body) = split_frontmatter(content);
    let mut entry = SkillEntry {
        name: dir_name.clone(),
        source,
        path: path.to_path_buf(),
        ..Default::default()
    };
    if let Some(yaml) = front {
        for line in yaml.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((k, v)) = line.split_once(':') else {
                continue;
            };
            let key = k.trim();
            let raw_value = v.trim();
            if raw_value.is_empty() {
                continue;
            }
            let value = strip_quotes(raw_value);
            if value.is_empty() {
                continue;
            }
            match key {
                "name" => entry.name = value.to_string(),
                "description" => entry.description = value.to_string(),
                "when_to_use" | "whenToUse" => entry.when_to_use = Some(value.to_string()),
                "argument_hint" | "argumentHint" => entry.argument_hint = Some(value.to_string()),
                "allowed_tools" | "allowedTools" | "allowed-tools" => {
                    entry.allowed_tools = Some(parse_yaml_list(value));
                }
                "disallowed_tools" | "disallowedTools" | "disallowed-tools" => {
                    entry.disallowed_tools = Some(parse_yaml_list(value));
                }
                "arguments" => {
                    entry.arguments = Some(parse_yaml_list(value));
                }
                "model" => entry.model = Some(value.to_string()),
                "effort" => entry.effort = Some(value.to_string()),
                "context" => entry.context = Some(value.to_string()),
                "agent" => entry.agent = Some(value.to_string()),
                "background" => {
                    entry.background = Some(!matches!(
                        value.to_lowercase().as_str(),
                        "false" | "no" | "0"
                    ));
                }
                "disable_model_invocation" | "disableModelInvocation" => {
                    entry.disable_model_invocation =
                        matches!(value.to_lowercase().as_str(), "true" | "yes" | "1");
                }
                "user_invocable" | "userInvocable" => {
                    entry.user_invocable =
                        !matches!(value.to_lowercase().as_str(), "false" | "no" | "0");
                }
                "paths" => {
                    entry.paths = Some(parse_yaml_list(value));
                }
                "version" => entry.version = Some(value.to_string()),
                "files" => {
                    entry.files = Some(parse_yaml_list(value));
                }
                "hooks" => {
                    entry.hooks = Some(parse_yaml_list(value));
                }
                _ => {}
            }
        }
    }
    if entry.description.is_empty() {
        entry.description = first_paragraph(body);
    }
    if entry.description.is_empty() {
        return None;
    }
    Some(entry)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_skill_extracts_all_three_fields() {
        let content = "---\nname: summarize-pr\ndescription: Summarize a PR\nwhen_to_use: When user asks for PR summary\n---\nbody\n";
        let entry = parse_skill_file(
            content,
            "summarize-pr".into(),
            Path::new("/x/SKILL.md"),
            SkillSource::User,
        )
        .unwrap();
        assert_eq!(entry.name, "summarize-pr");
        assert_eq!(entry.description, "Summarize a PR");
        assert_eq!(
            entry.when_to_use.as_deref(),
            Some("When user asks for PR summary")
        );
        assert_eq!(entry.source, SkillSource::User);
    }

    #[test]
    fn parse_skill_strips_quotes() {
        let content = "---\nname: foo\ndescription: \"with quotes\"\n---\nbody";
        let entry = parse_skill_file(
            content,
            "foo".into(),
            Path::new("/x/SKILL.md"),
            SkillSource::User,
        )
        .unwrap();
        assert_eq!(entry.description, "with quotes");
    }

    #[test]
    fn parse_skill_falls_back_to_first_body_line() {
        let content = "---\nname: foo\n---\n# Heading\n\nactual body";
        let entry = parse_skill_file(
            content,
            "foo".into(),
            Path::new("/x/SKILL.md"),
            SkillSource::Project,
        )
        .unwrap();
        // 取 `# Heading` -> 剥 # 后 `Heading`
        assert_eq!(entry.description, "Heading");
    }

    #[test]
    fn parse_skill_returns_none_when_no_description_anywhere() {
        let content = "---\nname: foo\n---\n\n\n";
        let entry = parse_skill_file(
            content,
            "foo".into(),
            Path::new("/x/SKILL.md"),
            SkillSource::User,
        );
        assert!(entry.is_none());
    }

    #[test]
    fn parse_skill_falls_back_to_first_paragraph_not_just_first_line() {
        // Multi-line opening paragraph, no leading heading — must join every
        // line up to the first blank line, not just the first one.
        let content = "---\nname: foo\n---\nThis is a skill\nthat does X.\n\nMore text below.";
        let entry = parse_skill_file(
            content,
            "foo".into(),
            Path::new("/x/SKILL.md"),
            SkillSource::User,
        )
        .unwrap();
        assert_eq!(entry.description, "This is a skill that does X.");
    }

    #[test]
    fn parse_yaml_list_accepts_space_separated_bare_string() {
        let content = "---\nname: foo\ndescription: d\nallowed-tools: Read Grep Glob\n---\nbody";
        let entry = parse_skill_file(
            content,
            "foo".into(),
            Path::new("/x/SKILL.md"),
            SkillSource::User,
        )
        .unwrap();
        assert_eq!(
            entry.allowed_tools,
            Some(vec![
                "Read".to_string(),
                "Grep".to_string(),
                "Glob".to_string()
            ])
        );
    }

    #[test]
    fn parse_skill_extracts_disallowed_tools_and_arguments() {
        let content = "---\nname: foo\ndescription: d\ndisallowed-tools: AskUserQuestion\narguments: issue, branch\neffort: high\nagent: Explore\nbackground: false\n---\nbody";
        let entry = parse_skill_file(
            content,
            "foo".into(),
            Path::new("/x/SKILL.md"),
            SkillSource::User,
        )
        .unwrap();
        assert_eq!(
            entry.disallowed_tools,
            Some(vec!["AskUserQuestion".to_string()])
        );
        assert_eq!(
            entry.arguments,
            Some(vec!["issue".to_string(), "branch".to_string()])
        );
        assert_eq!(entry.effort.as_deref(), Some("high"));
        assert_eq!(entry.agent.as_deref(), Some("Explore"));
        assert_eq!(entry.background, Some(false));
    }

    #[test]
    fn parse_skill_uses_dir_name_when_no_name_in_frontmatter() {
        let content = "---\ndescription: A skill\n---\nbody";
        let entry = parse_skill_file(
            content,
            "my-skill".into(),
            Path::new("/x/SKILL.md"),
            SkillSource::User,
        )
        .unwrap();
        assert_eq!(entry.name, "my-skill");
    }
}
