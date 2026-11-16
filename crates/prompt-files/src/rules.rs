//! The `<project dir>/rules/` directories as `RuleProvider`s: one directory of `.md`
//! files, and the engine's global, scene and project tiers of them.

use base::interface::instruction_provider::RuleProvider;
use base::paths::{ConfigPaths, PathSettings, PromptFiles};
use base::rules::{RuleEntry, RuleTiers};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub const MAX_RULE_DESCRIPTION_CHARS: usize = 150;

pub struct RuleDirectory {
    dir: PathBuf,
    id: String,
    files: PromptFiles,
}

impl RuleDirectory {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        let dir = dir.into();
        let id = dir.display().to_string();
        Self {
            dir,
            id,
            files: PromptFiles::unscoped(),
        }
    }

    pub fn reading_with(mut self, files: PromptFiles) -> Self {
        self.files = files;
        self
    }
}

impl RuleProvider for RuleDirectory {
    fn id(&self) -> &str {
        &self.id
    }

    fn rules(&self) -> Vec<RuleEntry> {
        scan_rules_dir(&self.dir, &self.files)
    }
}

/// Outermost first, so project overrides scene overrides global — the same
/// precedence as skills and agents.
pub fn rule_tiers(paths: &PathSettings) -> RuleTiers {
    let files = PromptFiles::for_project(&ConfigPaths::from_settings(paths));
    let dirs = paths.config_paths();
    let sources = [dirs.global_rules_dir(), dirs.user_rules_dir()]
        .into_iter()
        .chain(dirs.project_rules_dir())
        .map(|dir| {
            Arc::new(RuleDirectory::new(dir).reading_with(files.clone())) as Arc<dyn RuleProvider>
        })
        .collect();
    RuleTiers::new(sources)
}

fn scan_rules_dir(dir: &Path, files: &PromptFiles) -> Vec<RuleEntry> {
    let mut out = Vec::new();
    let Ok(read_dir) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in read_dir.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("md") {
            continue;
        }
        let Some(name) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let description = match files.read_to_string(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => continue,
            read => read
                .ok()
                .and_then(|content| first_line_description(&content))
                .unwrap_or_else(|| "(no description)".to_string()),
        };
        out.push(RuleEntry {
            name: name.to_string(),
            description,
            path,
        });
    }
    out
}

/// First non-empty, non-bare-`#` line, stripped and capped.
pub fn first_line_description(content: &str) -> Option<String> {
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let stripped = trimmed.trim_start_matches('#').trim();
        if stripped.is_empty() {
            continue;
        }
        let truncated: String = stripped.chars().take(MAX_RULE_DESCRIPTION_CHARS).collect();
        return Some(if stripped.chars().count() > MAX_RULE_DESCRIPTION_CHARS {
            format!("{truncated}...")
        } else {
            truncated
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use base::paths::{Layout, Workspace};
    use tempfile::TempDir;

    fn write_rule(dir: &Path, filename: &str, content: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(filename), content).unwrap();
    }

    fn layouts() -> [Layout; 2] {
        [Layout::default(), Layout::new(".codix").unwrap()]
    }

    fn paths_for(project: &Path, scene: &Path, global: &Path, layout: &Layout) -> PathSettings {
        let local = layout.project_dir(project);
        PathSettings::new(
            scene.to_path_buf(),
            global.to_path_buf(),
            local.clone(),
            Workspace::project_of(&local),
            "code".into(),
            layout.clone(),
        )
    }

    #[test]
    fn no_rules_when_no_dirs_exist() {
        for layout in layouts() {
            let root = TempDir::new().unwrap();
            let paths = paths_for(
                &root.path().join("project"),
                &root.path().join("scene"),
                &root.path().join("global"),
                &layout,
            );
            assert!(rule_tiers(&paths).rules().is_empty());
        }
    }

    #[test]
    fn the_first_line_is_the_description() {
        for layout in layouts() {
            let root = TempDir::new().unwrap();
            let project = root.path().join("project");
            write_rule(
                &layout.project_dir(&project).join("rules"),
                "security.md",
                "# Security guidelines\n\nBody text here.",
            );
            let paths = paths_for(
                &project,
                &root.path().join("scene"),
                &root.path().join("global"),
                &layout,
            );
            let entries = rule_tiers(&paths).rules();
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].name, "security");
            assert_eq!(entries[0].description, "Security guidelines");
        }
    }

    #[test]
    fn project_overrides_scene_overrides_global() {
        for layout in layouts() {
            let root = TempDir::new().unwrap();
            let project = root.path().join("project");
            let scene = root.path().join("scene");
            let global = root.path().join("global");
            write_rule(&global.join("rules"), "testing.md", "from global");
            write_rule(&scene.join("rules"), "testing.md", "from scene");
            write_rule(
                &layout.project_dir(&project).join("rules"),
                "testing.md",
                "from project",
            );

            let entries = rule_tiers(&paths_for(&project, &scene, &global, &layout)).rules();
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].description, "from project");
        }
    }

    #[test]
    fn distinct_names_from_different_tiers_all_show() {
        for layout in layouts() {
            let root = TempDir::new().unwrap();
            let project = root.path().join("project");
            let global = root.path().join("global");
            write_rule(&global.join("rules"), "architecture.md", "global rule");
            write_rule(
                &layout.project_dir(&project).join("rules"),
                "security.md",
                "project rule",
            );

            let paths = paths_for(&project, &root.path().join("scene"), &global, &layout);
            let entries = rule_tiers(&paths).rules();
            let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
            assert_eq!(names, ["architecture", "security"]);
        }
    }

    #[test]
    fn a_rule_directory_reads_only_md_files() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("notes.txt"), "ignored").unwrap();
        std::fs::write(dir.path().join("security.md"), "# Security\n").unwrap();
        let rules = RuleDirectory::new(dir.path()).rules();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].name, "security");
    }

    #[test]
    fn a_missing_rule_directory_is_not_an_error() {
        assert!(RuleDirectory::new("/nonexistent/rules").rules().is_empty());
    }

    #[test]
    fn first_line_description_truncates_long_lines() {
        let long = "a".repeat(300);
        let desc = first_line_description(&long).unwrap();
        assert!(desc.ends_with("..."));
        assert!(desc.chars().count() <= MAX_RULE_DESCRIPTION_CHARS + 3);
    }

    #[test]
    fn first_line_description_skips_blank_lines_and_bare_hashes() {
        let content = "\n\n#  \n# Real Title\nbody";
        assert_eq!(
            first_line_description(content).as_deref(),
            Some("Real Title")
        );
    }
}
