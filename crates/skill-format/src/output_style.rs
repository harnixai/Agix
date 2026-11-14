//! Output styles: a named markdown file that changes how the model writes.

use base::frozen::{OutputStyle, OutputStyleSource};
use base::paths::{ConfigPaths, PromptFiles};
use base::text::truncate_chars;
use std::path::Path;

/// Load `<project>/.agix/output-styles/<name>.md` if present,
/// else `~/.agix/scenes/<scope>/output-styles/<name>.md`. Trims whitespace and caps body
/// at 8KB. Returns None if neither file is present or readable.
pub fn load_output_style(
    project: Option<&Path>,
    name: &str,
    paths: &ConfigPaths,
    files: &PromptFiles,
) -> Option<OutputStyle> {
    let safe = name.trim();
    if safe.is_empty() || safe.contains('/') || safe.contains('\\') || safe.starts_with('.') {
        return None;
    }
    let project = project.map(|p| {
        p.join(".agix")
            .join("output-styles")
            .join(format!("{safe}.md"))
    });
    if let Some(Ok(content)) = project.as_deref().map(|path| files.read_to_string(path)) {
        if !content.trim().is_empty() {
            return Some(OutputStyle {
                name: safe.to_string(),
                source: OutputStyleSource::Project,
                content: truncate_chars(content.trim(), 8_000, "\n... (output style truncated)"),
            });
        }
    }
    {
        let user = paths
            .user_data_dir
            .join("output-styles")
            .join(format!("{safe}.md"));
        if let Ok(content) = files.read_to_string(&user) {
            if !content.trim().is_empty() {
                return Some(OutputStyle {
                    name: safe.to_string(),
                    source: OutputStyleSource::User,
                    content: truncate_chars(
                        content.trim(),
                        8_000,
                        "\n... (output style truncated)",
                    ),
                });
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn test_paths(root: &Path) -> ConfigPaths {
        ConfigPaths::new(root.join(".agix"), root.join(".agix"), "code")
    }

    #[test]
    fn output_style_project_takes_precedence_over_user() {
        let home = TempDir::new().unwrap();
        let cwd = TempDir::new().unwrap();
        let user_dir = test_paths(home.path()).user_data_dir.join("output-styles");
        let proj_dir = cwd.path().join(".agix/output-styles");
        std::fs::create_dir_all(&user_dir).unwrap();
        std::fs::create_dir_all(&proj_dir).unwrap();
        std::fs::write(user_dir.join("terse.md"), "USER VERSION").unwrap();
        std::fs::write(proj_dir.join("terse.md"), "PROJECT VERSION").unwrap();

        let style = load_output_style(
            Some(cwd.path()),
            "terse",
            &test_paths(home.path()),
            &PromptFiles::unscoped(),
        )
        .unwrap();
        assert_eq!(style.name, "terse");
        assert_eq!(style.source, OutputStyleSource::Project);
        assert!(style.content.contains("PROJECT VERSION"));
    }

    #[test]
    fn output_style_falls_back_to_user_when_no_project() {
        let home = TempDir::new().unwrap();
        let cwd = TempDir::new().unwrap();
        let user_dir = test_paths(home.path()).user_data_dir.join("output-styles");
        std::fs::create_dir_all(&user_dir).unwrap();
        std::fs::write(user_dir.join("verbose.md"), "explain everything").unwrap();

        let style = load_output_style(
            Some(cwd.path()),
            "verbose",
            &test_paths(home.path()),
            &PromptFiles::unscoped(),
        )
        .unwrap();
        assert_eq!(style.source, OutputStyleSource::User);
        assert!(style.content.contains("explain everything"));
    }

    #[test]
    fn output_style_returns_none_for_path_traversal_attempt() {
        let cwd = TempDir::new().unwrap();
        // names with slashes / leading dot must not load anything
        assert!(load_output_style(
            Some(cwd.path()),
            "../etc/passwd",
            &test_paths(cwd.path()),
            &PromptFiles::unscoped()
        )
        .is_none());
        assert!(load_output_style(
            Some(cwd.path()),
            ".hidden",
            &test_paths(cwd.path()),
            &PromptFiles::unscoped()
        )
        .is_none());
        assert!(load_output_style(
            Some(cwd.path()),
            "",
            &test_paths(cwd.path()),
            &PromptFiles::unscoped()
        )
        .is_none());
    }
}
