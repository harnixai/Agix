//! Output styles: a named markdown file that changes how the model writes.

use base::frozen::{OutputStyle, OutputStyleSource};
use base::paths::{entry, ConfigPaths, PromptFiles};
use base::text::truncate_chars;
use std::path::Path;

/// Load `<project>/<project dir>/output-styles/<name>.md` if present,
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
    let from_project = || {
        let path = paths
            .layout
            .project_dir(project?)
            .join(entry::OUTPUT_STYLES)
            .join(format!("{safe}.md"));
        let content = files.read_to_string(&path).ok()?;
        style_of(safe, OutputStyleSource::Project, &content)
    };
    let from_user = || {
        let path = paths.user_output_styles_dir().join(format!("{safe}.md"));
        let content = files.read_to_string(&path).ok()?;
        style_of(safe, OutputStyleSource::User, &content)
    };
    if paths.project_untrusted {
        from_user().or_else(from_project)
    } else {
        from_project().or_else(from_user)
    }
}

fn style_of(name: &str, source: OutputStyleSource, content: &str) -> Option<OutputStyle> {
    (!content.trim().is_empty()).then(|| OutputStyle {
        name: name.to_string(),
        source,
        content: truncate_chars(content.trim(), 8_000, "\n... (output style truncated)"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use base::paths::Layout;
    use tempfile::TempDir;

    fn layouts() -> [Layout; 2] {
        [Layout::default(), Layout::new(".codix").unwrap()]
    }

    fn test_paths(root: &Path, layout: &Layout) -> ConfigPaths {
        ConfigPaths::new(
            root.join(".state"),
            layout.project_dir(root),
            "code",
            layout.clone(),
        )
    }

    #[test]
    fn output_style_project_takes_precedence_over_user() {
        for layout in layouts() {
            let home = TempDir::new().unwrap();
            let cwd = TempDir::new().unwrap();
            let user_dir = test_paths(home.path(), &layout)
                .user_data_dir
                .join("output-styles");
            let proj_dir = layout.project_dir(cwd.path()).join("output-styles");
            std::fs::create_dir_all(&user_dir).unwrap();
            std::fs::create_dir_all(&proj_dir).unwrap();
            std::fs::write(user_dir.join("terse.md"), "USER VERSION").unwrap();
            std::fs::write(proj_dir.join("terse.md"), "PROJECT VERSION").unwrap();

            let style = load_output_style(
                Some(cwd.path()),
                "terse",
                &test_paths(home.path(), &layout),
                &PromptFiles::unscoped(),
            )
            .unwrap();
            assert_eq!(style.name, "terse");
            assert_eq!(style.source, OutputStyleSource::Project);
            assert!(style.content.contains("PROJECT VERSION"));
        }
    }

    #[test]
    fn an_untrusted_projects_output_style_does_not_replace_the_users() {
        for layout in layouts() {
            let home = TempDir::new().unwrap();
            let cwd = TempDir::new().unwrap();
            let mut paths = test_paths(home.path(), &layout);
            paths.project_untrusted = true;
            let user_dir = paths.user_data_dir.join("output-styles");
            let proj_dir = layout.project_dir(cwd.path()).join("output-styles");
            std::fs::create_dir_all(&user_dir).unwrap();
            std::fs::create_dir_all(&proj_dir).unwrap();
            std::fs::write(user_dir.join("terse.md"), "USER VERSION").unwrap();
            std::fs::write(proj_dir.join("terse.md"), "PROJECT VERSION").unwrap();
            std::fs::write(proj_dir.join("extra.md"), "PROJECT ONLY").unwrap();

            let files = PromptFiles::unscoped();
            let terse = load_output_style(Some(cwd.path()), "terse", &paths, &files).unwrap();
            assert_eq!(terse.source, OutputStyleSource::User);
            let extra = load_output_style(Some(cwd.path()), "extra", &paths, &files).unwrap();
            assert_eq!(extra.source, OutputStyleSource::Project);
        }
    }

    #[test]
    fn output_style_falls_back_to_user_when_no_project() {
        for layout in layouts() {
            let home = TempDir::new().unwrap();
            let cwd = TempDir::new().unwrap();
            let user_dir = test_paths(home.path(), &layout)
                .user_data_dir
                .join("output-styles");
            std::fs::create_dir_all(&user_dir).unwrap();
            std::fs::write(user_dir.join("verbose.md"), "explain everything").unwrap();

            let style = load_output_style(
                Some(cwd.path()),
                "verbose",
                &test_paths(home.path(), &layout),
                &PromptFiles::unscoped(),
            )
            .unwrap();
            assert_eq!(style.source, OutputStyleSource::User);
            assert!(style.content.contains("explain everything"));
        }
    }

    #[test]
    fn output_style_returns_none_for_path_traversal_attempt() {
        for layout in layouts() {
            let cwd = TempDir::new().unwrap();
            // names with slashes / leading dot must not load anything
            assert!(load_output_style(
                Some(cwd.path()),
                "../etc/passwd",
                &test_paths(cwd.path(), &layout),
                &PromptFiles::unscoped()
            )
            .is_none());
            assert!(load_output_style(
                Some(cwd.path()),
                ".hidden",
                &test_paths(cwd.path(), &layout),
                &PromptFiles::unscoped()
            )
            .is_none());
            assert!(load_output_style(
                Some(cwd.path()),
                "",
                &test_paths(cwd.path(), &layout),
                &PromptFiles::unscoped()
            )
            .is_none());
        }
    }
}
