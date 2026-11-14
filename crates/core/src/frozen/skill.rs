//! Skill loading, expansion, and activation.
//!
//! Skills are loaded from three tiers: `~/.agix/skills/<name>/SKILL.md`
//! (global default, shared by every scene), `~/.agix/scenes/<scope>/skills/<name>/SKILL.md`
//! (scene-specific override — same name wins over the global default; `scope`
//! identifies the product instance, no default), and
//! `<cwd>/.agents/skills/<name>/SKILL.md` (project-level — this is the
//! external fact standard also scanned by Codex). They can be invoked via
//! slash commands (`/<name> [args]`) or conditionally activated when
//! matching file paths are touched.

use std::path::{Path, PathBuf};

use super::frontmatter::parse_skill_file;
use super::regex::Regex;
use crate::skill::{expand_skill_vars_named, InvocationVars, SkillEntry, SkillSource};
use crate::text::frontmatter::split_frontmatter;

impl SkillEntry {
    /// 读 SKILL.md，剥 frontmatter，返回 markdown body（trim 后）。
    /// 给 `/<skill-name>` slash 调用时把 body 作为 user prompt 模板用。
    ///
    /// 特殊路径 `(bundled:<name>)` 走内置 skill 的 in-memory body 而非读盘。
    pub async fn read_body(&self) -> std::io::Result<String> {
        let path_str = self.path.to_string_lossy();
        if let Some(name) = path_str
            .strip_prefix("(bundled:")
            .and_then(|s| s.strip_suffix(")"))
        {
            // Bundled skill bodies need a resolver from the skills crate, which
            // core cannot depend on, so a bundled prefix is reported NotFound.
            if path_str.starts_with("(bundled:") {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("bundled skill resolver not configured for: {name}"),
                ));
            }
        }
        let content = crate::paths::PromptFiles::unscoped().read_to_string(&self.path)?;
        let (_front, body) = split_frontmatter(&content);
        Ok(body.trim().to_string())
    }
}

/// 从三层扫 SKILL.md：全局 `~/.agix/skills/` -> scene 覆盖
/// `~/.agix/scenes/<scope>/skills/` -> 项目级 `<project>/.agents/skills/`（外部事实标准，
/// 无项目会话没有这一层）。
/// 同名时后扫到的覆盖先扫到的（`scan_skills_dir` 结果直接 extend 进 `Vec`，
/// 调用方按"先入者优先"处理 slash 分发，所以顺序是：scene 覆盖需要排在它要
/// 覆盖的全局条目**之前**，让 scene 版本先命中）。
async fn collect_skills(
    paths: &crate::paths::ConfigPaths,
    project: Option<&Path>,
) -> Vec<SkillEntry> {
    let global_dir = paths.global_skills_dir();
    let scene_dir = paths.user_skills_dir();
    let files = crate::paths::PromptFiles::for_project(paths);
    let mut all = Vec::new();
    all.extend(scan_skills_dir(&scene_dir, SkillSource::User, &files).await);
    all.extend(scan_skills_dir(&global_dir, SkillSource::User, &files).await);
    if let Some(project) = project {
        let project_dir = project.join(".agents").join("skills");
        all.extend(scan_skills_dir(&project_dir, SkillSource::Project, &files).await);
    }
    all
}

async fn scan_skills_dir(
    dir: &Path,
    source: SkillSource,
    files: &crate::paths::PromptFiles,
) -> Vec<SkillEntry> {
    let mut out = Vec::new();
    let mut entries = match tokio::fs::read_dir(dir).await {
        Ok(e) => e,
        Err(_) => return out,
    };
    let mut subdirs: Vec<PathBuf> = Vec::new();
    while let Ok(Some(e)) = entries.next_entry().await {
        let p = e.path();
        if p.is_dir() {
            subdirs.push(p);
        }
    }
    subdirs.sort();
    for d in subdirs {
        let skill_md = d.join("SKILL.md");
        let Ok(content) = files.read_to_string(&skill_md) else {
            continue;
        };
        let dir_name = d
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "(unnamed)".to_string());
        if let Some(entry) = parse_skill_file(&content, dir_name, &skill_md, source) {
            out.push(entry);
        }
    }
    out
}

/// Public helper used by plugin loader to convert a SKILL.md path into
/// a [`SkillEntry`]. Reads the file, parses YAML frontmatter, falls back to
/// first markdown body line for description.
pub async fn load_skill_from_path(path: &Path, source: SkillSource) -> Option<SkillEntry> {
    let content = crate::paths::PromptFiles::unscoped()
        .read_to_string(path)
        .ok()?;
    let dir_name = path
        .parent()
        .and_then(|p| p.file_name())
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "plugin-skill".to_string());
    parse_skill_file(&content, dir_name, path, source)
}

/// 公开的"只扫 skills"入口 -- 给 CLI/TUI 启动时构造 skill 列表用，避免重跑
/// FrozenContext::collect 里的 git 子命令。
///
/// 把 5 个内置 bundled skills 追加到列表末尾。disk 上同名 skill 优先
/// （因为先入列表，slash 命中第一个）。
pub async fn load_session_skills(
    project: Option<&Path>,
    paths: &crate::paths::ConfigPaths,
) -> Vec<SkillEntry> {
    // Disk skills are loaded first (take priority for slash commands).
    // Callers should use collect_skills_with_bundled() to append bundled skills.
    collect_skills(paths, project).await
}

/// Like [`load_session_skills`] but appends bundled skills after disk skills.
/// Callers in the `runtime` or `skills` crate pass in bundled skill entries
/// (e.g. from `skills::bundled::bundled_skills()`).
///
/// Disk skills appear first, so same-name slash commands hit the user's version.
/// Bundled skills that don't exist on disk are appended last.
pub async fn load_session_skills_with_bundled(
    project: Option<&Path>,
    bundled: Vec<SkillEntry>,
    paths: &crate::paths::ConfigPaths,
) -> Vec<SkillEntry> {
    let mut all = load_session_skills(project, paths).await;
    let disk_names: std::collections::HashSet<String> =
        all.iter().map(|s| s.name.clone()).collect();
    for s in bundled {
        if !disk_names.contains(&s.name) {
            all.push(s);
        }
    }
    all
}

/// Activate conditional skills whose `paths` globs match the given file paths.
///
/// Conditional skills are those with a `paths` frontmatter field listing glob
/// patterns. When a tool touches files matching those patterns, the skill becomes
/// available to the model. Activation is session-persistent (once activated,
/// stays active).
pub fn activate_conditional_skills(
    all_skills: &mut Vec<SkillEntry>,
    conditional_skills: &mut Vec<SkillEntry>,
    affected_paths: &[&str],
) {
    if conditional_skills.is_empty() || affected_paths.is_empty() {
        return;
    }
    let mut activated_indices: Vec<usize> = Vec::new();
    for (i, skill) in conditional_skills.iter().enumerate() {
        if let Some(ref patterns) = skill.paths {
            let mut matched = false;
            for path_str in affected_paths {
                for pat in patterns {
                    // Simple glob matching: supports * and ** wildcards
                    if simple_glob_match(pat, path_str) {
                        matched = true;
                        break;
                    }
                }
                if matched {
                    break;
                }
            }
            if matched {
                activated_indices.push(i);
            }
        }
    }
    // Move activated skills from conditional to active (reverse order for safe removal)
    for &i in activated_indices.iter().rev() {
        let skill = conditional_skills.remove(i);
        all_skills.push(skill);
    }
}

/// Simple glob matching supporting `*` (single-segment) and `**` (multi-segment) wildcards.
/// Used for conditional skill path matching.
fn simple_glob_match(pattern: &str, path: &str) -> bool {
    let pattern = pattern.trim();
    if pattern.is_empty() {
        return false;
    }
    // Literal substring match
    if !pattern.contains('*') {
        return path.contains(pattern);
    }
    // Convert glob to check-each-segment
    let pat_segments: Vec<&str> = pattern.split('/').collect();
    let path_segments: Vec<&str> = path.split('/').collect();

    fn match_segments(pat: &[&str], p: &[&str], pi: usize, si: usize) -> bool {
        if pi >= pat.len() {
            return si >= p.len();
        }
        if si >= p.len() {
            // Only ** can match empty
            return pat[pi..].iter().all(|s| *s == "**");
        }
        match pat[pi] {
            "**" => {
                // ** matches zero or more segments
                match_segments(pat, p, pi + 1, si) // zero segments
                    || match_segments(pat, p, pi + 1, si + 1) // one segment
                    || match_segments(pat, p, pi, si + 1) // more segments
            }
            "*" => match_segments(pat, p, pi + 1, si + 1),
            literal => {
                if literal == p[si] {
                    match_segments(pat, p, pi + 1, si + 1)
                } else if literal.contains('*') {
                    // Handle partial wildcards like "*.rs" or "foo*.txt"
                    let re_str = format!("^{}$", regex_escape(literal).replace("\\*", ".*"));
                    if let Ok(re) = Regex::new(&re_str) {
                        re.is_match(p[si]) && match_segments(pat, p, pi + 1, si + 1)
                    } else {
                        false
                    }
                } else {
                    false
                }
            }
        }
    }

    match_segments(&pat_segments, &path_segments, 0, 0)
}

fn regex_escape(s: &str) -> String {
    let mut escaped = String::new();
    for c in s.chars() {
        if ".^$+?()[]{}|\\".contains(c) {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    escaped
}

/// 尝试把 `/<skill-name> [args]` 这类 slash 输入展开成 user prompt：
///
/// - `line` 不以 `/` 开头 -> 立即 None
/// - `name` 不在 skills 列表 -> None（让 caller 走原本 slash 分派 / Unknown 流程）
/// - 命中：异步读 SKILL.md，剥 frontmatter，返回拼好的 prompt（含 args 透传）
///
/// args 拼接策略简单：`$ARGUMENTS` 占位符替换；没占位符则在 body 末尾追加。
/// 这是 first cut，刻意不实现 `$1` 等完整变量代换 -- 用户场景里现在不卡这个。
pub async fn try_expand_skill_command(
    line: &str,
    skills: &[SkillEntry],
) -> Option<Result<String, std::io::Error>> {
    let trimmed = line.trim();
    // Support both /skill and !skill prefix conventions
    let stripped = trimmed
        .strip_prefix('/')
        .or_else(|| trimmed.strip_prefix('!'))?;
    let (cmd, args) = match stripped.split_once(char::is_whitespace) {
        Some((c, a)) => (c, a.trim()),
        None => (stripped, ""),
    };
    let skill = skills.iter().find(|s| s.name == cmd)?;
    Some(format_skill_invocation(skill, args).await)
}

pub fn expand_skill_vars(body: &str, args: &str) -> String {
    expand_skill_vars_named(body, args, &[], None)
}

pub async fn format_skill_invocation(
    skill: &SkillEntry,
    args: &str,
) -> Result<String, std::io::Error> {
    let body = skill.read_body().await?;
    // Variable expansion supports:
    //   - `$ARGUMENTS` -- full args string
    //   - `$1`..`$9` -- positional args split on whitespace
    // If none of the above are present and args is non-empty, args is appended
    // verbatim at the end (legacy behavior).
    let body_with_args = expand_skill_vars_named(
        &body,
        args,
        skill.arguments.as_deref().unwrap_or(&[]),
        Some(&InvocationVars {
            skill_md: &skill.path,
            session_id: None,
        }),
    );
    let mut s = String::with_capacity(body_with_args.len() + 256);
    s.push_str(&format!(
        "Apply the skill `{}` ({}). Follow the playbook below.\n\n",
        skill.name, skill.description
    ));
    if let Some(w) = &skill.when_to_use {
        s.push_str(&format!("When to use: {w}\n\n"));
    }
    s.push_str("---\n\n");
    s.push_str(&body_with_args);
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Roots under a tempdir — the same shape a real instance gets, minus any
    /// dependence on who is running the test.
    fn test_paths(home: &Path) -> crate::paths::ConfigPaths {
        crate::paths::ConfigPaths::new(home.join(".agix"), home.join("project/.agix"), "code")
    }
    #[tokio::test]
    async fn collect_skills_loads_user_and_project() {
        let home = TempDir::new().unwrap();
        let cwd = TempDir::new().unwrap();
        // scene-specific skill
        let u_dir = home.path().join(".agix/scenes/code/skills/u-skill");
        tokio::fs::create_dir_all(&u_dir).await.unwrap();
        tokio::fs::write(
            u_dir.join("SKILL.md"),
            "---\ndescription: from user\n---\nbody",
        )
        .await
        .unwrap();
        // project skill — lives under the new authoritative `.agents/skills/`
        let p_dir = cwd.path().join(".agents/skills/p-skill");
        tokio::fs::create_dir_all(&p_dir).await.unwrap();
        tokio::fs::write(
            p_dir.join("SKILL.md"),
            "---\ndescription: from project\n---\nbody",
        )
        .await
        .unwrap();
        let skills = collect_skills(&test_paths(home.path()), Some(cwd.path())).await;
        assert_eq!(skills.len(), 2);
        // user (scene) 在前
        assert_eq!(skills[0].name, "u-skill");
        assert_eq!(skills[0].source, SkillSource::User);
        assert_eq!(skills[1].name, "p-skill");
        assert_eq!(skills[1].source, SkillSource::Project);
    }

    #[tokio::test]
    async fn collect_skills_scene_overrides_global_default_by_name() {
        let home = TempDir::new().unwrap();
        let cwd = TempDir::new().unwrap();
        let global_dir = home.path().join(".agix/skills/shared");
        tokio::fs::create_dir_all(&global_dir).await.unwrap();
        tokio::fs::write(
            global_dir.join("SKILL.md"),
            "---\ndescription: global version\n---\nbody",
        )
        .await
        .unwrap();
        let scene_dir = home.path().join(".agix/scenes/code/skills/shared");
        tokio::fs::create_dir_all(&scene_dir).await.unwrap();
        tokio::fs::write(
            scene_dir.join("SKILL.md"),
            "---\ndescription: scene override\n---\nbody",
        )
        .await
        .unwrap();

        let skills = collect_skills(&test_paths(home.path()), Some(cwd.path())).await;
        let shared: Vec<_> = skills.iter().filter(|s| s.name == "shared").collect();
        // Both entries are present in the raw list (same-name-both-kept
        // convention, see `SkillSource` docs) but the scene one comes first,
        // so callers that pick "first match wins" for slash dispatch get the
        // scene-specific version.
        assert_eq!(shared.len(), 2);
        assert_eq!(shared[0].description, "scene override");
    }

    #[tokio::test]
    async fn collect_skills_skips_dir_without_skill_md() {
        let home = TempDir::new().unwrap();
        let cwd = TempDir::new().unwrap();
        // 目录存在但没有 SKILL.md
        let d = home.path().join(".agix/scenes/code/skills/empty");
        tokio::fs::create_dir_all(&d).await.unwrap();
        let skills = collect_skills(&test_paths(home.path()), Some(cwd.path())).await;
        assert!(skills.is_empty());
    }

    #[tokio::test]
    async fn collect_skills_ignores_old_agix_code_skills_path() {
        // Pre-release, no back-compat: `.agix/code/skills/` is not scanned at
        // all (only `.agents/skills/` is the project-level source of truth).
        let home = TempDir::new().unwrap();
        let cwd = TempDir::new().unwrap();
        let old_dir = cwd.path().join(".agix/code/skills/old-skill");
        tokio::fs::create_dir_all(&old_dir).await.unwrap();
        tokio::fs::write(
            old_dir.join("SKILL.md"),
            "---\ndescription: old path\n---\nbody",
        )
        .await
        .unwrap();

        let skills = collect_skills(&test_paths(home.path()), Some(cwd.path())).await;
        assert!(
            skills.is_empty(),
            "`.agix/code/skills/` must not be scanned"
        );
    }

    #[tokio::test]
    async fn skill_read_body_strips_frontmatter() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("SKILL.md");
        tokio::fs::write(
            &p,
            "---\nname: foo\ndescription: bar\n---\nThis is the body.\nLine 2.\n",
        )
        .await
        .unwrap();
        let skill = SkillEntry {
            name: "foo".into(),
            description: "bar".into(),
            when_to_use: None,
            source: SkillSource::User,
            path: p,
            ..Default::default()
        };
        let body = skill.read_body().await.unwrap();
        assert!(body.contains("This is the body."));
        assert!(body.contains("Line 2."));
        assert!(!body.contains("---"));
        assert!(!body.contains("description"));
    }

    #[tokio::test]
    async fn try_expand_returns_none_for_unknown_skill() {
        let result = try_expand_skill_command("/ghost", &[]).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn try_expand_returns_none_for_non_slash() {
        let result = try_expand_skill_command("plain text", &[]).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn try_expand_skill_inlines_body_with_invocation_header() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("SKILL.md");
        tokio::fs::write(
            &p,
            "---\nname: summarize\ndescription: Summarize a PR\nwhen_to_use: when asked\n---\nRead the diff. Summarize in 3 bullets.",
        )
        .await
        .unwrap();
        let skills = vec![SkillEntry {
            name: "summarize".into(),
            description: "Summarize a PR".into(),
            when_to_use: Some("when asked".into()),
            source: SkillSource::User,
            path: p,
            ..Default::default()
        }];
        let result = try_expand_skill_command("/summarize", &skills)
            .await
            .expect("skill matched")
            .unwrap();
        assert!(result.contains("Apply the skill `summarize`"));
        assert!(result.contains("Summarize a PR"));
        assert!(result.contains("when asked"));
        assert!(result.contains("Read the diff. Summarize in 3 bullets."));
        assert!(
            !result.contains("---\nname:"),
            "frontmatter must be stripped"
        );
    }

    #[tokio::test]
    async fn try_expand_appends_args_when_no_placeholder() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("SKILL.md");
        tokio::fs::write(&p, "---\ndescription: test\n---\nbody without placeholder")
            .await
            .unwrap();
        let skills = vec![SkillEntry {
            name: "x".into(),
            description: "test".into(),
            when_to_use: None,
            source: SkillSource::User,
            path: p,
            ..Default::default()
        }];
        let result = try_expand_skill_command("/x focus on auth", &skills)
            .await
            .unwrap()
            .unwrap();
        assert!(result.contains("body without placeholder"));
        assert!(result.contains("User arguments: focus on auth"));
    }

    #[test]
    fn expand_dollar_arguments_replaces_full() {
        let body = "Review $ARGUMENTS now.";
        assert_eq!(
            expand_skill_vars(body, "feature/auth"),
            "Review feature/auth now."
        );
    }

    #[test]
    fn expand_dollar_positional_args() {
        let body = "Compare $1 to $2 ignoring $3.";
        assert_eq!(
            expand_skill_vars(body, "main develop trailing"),
            "Compare main to develop ignoring trailing."
        );
    }

    #[test]
    fn expand_missing_positional_becomes_empty() {
        let body = "first=$1 second=$2";
        assert_eq!(expand_skill_vars(body, "only"), "first=only second=");
    }

    #[test]
    fn expand_no_placeholder_falls_back_to_appendix() {
        let body = "Generic skill body.";
        let r = expand_skill_vars(body, "extra context");
        assert!(r.contains("Generic skill body."));
        assert!(r.contains("User arguments: extra context"));
    }

    #[test]
    fn expand_no_placeholder_empty_args_returns_body_unchanged() {
        assert_eq!(expand_skill_vars("body", ""), "body");
    }

    #[test]
    fn expand_dollar_arguments_overrides_appendix() {
        let body = "Argument: $ARGUMENTS";
        let r = expand_skill_vars(body, "go");
        assert_eq!(r, "Argument: go");
        assert!(!r.contains("User arguments:"));
    }

    #[tokio::test]
    async fn try_expand_substitutes_args_placeholder() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("SKILL.md");
        tokio::fs::write(
            &p,
            "---\ndescription: test\n---\nReview $ARGUMENTS and report.",
        )
        .await
        .unwrap();
        let skills = vec![SkillEntry {
            name: "x".into(),
            description: "test".into(),
            when_to_use: None,
            source: SkillSource::User,
            path: p,
            ..Default::default()
        }];
        let result = try_expand_skill_command("/x PR #42", &skills)
            .await
            .unwrap()
            .unwrap();
        assert!(result.contains("Review PR #42 and report."));
        assert!(!result.contains("$ARGUMENTS"));
        assert!(!result.contains("User arguments:"));
    }
}
