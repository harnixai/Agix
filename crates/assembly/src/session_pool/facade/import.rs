//! 从其他工具导入配置：可导入的来源与一次导入。

use crate::session_pool::*;

impl SessionPool {
    /// Detect importable cross-tool configuration sources (Claude Code/
    /// Codex/Cursor) in this pool's project directory — pure filesystem
    /// detection, no LLM turn needed. The manual `/import` path (the bundled
    /// `import` skill) does its own detection with the file tools.
    pub async fn list_import_sources(&self, project_root: Option<&Path>) -> Vec<api::ImportSource> {
        let project_root = project_root.unwrap_or(self.cwd());
        settings::import::detect_import_sources(project_root)
            .await
            .iter()
            .map(|s| api::ImportSource {
                source: s.kind().as_str().to_string(),
                description: s.describe(),
            })
            .collect()
    }

    /// Execute an import for a previously-detected `source` (one of the
    /// `source` values `list_import_sources` returns) and record the
    /// decision so the automatic startup detection doesn't ask again.
    pub async fn run_import(
        &self,
        project_root: Option<&Path>,
        source: &str,
    ) -> Result<api::ImportRun, PoolError> {
        let invalid = |message: String| PoolError::InvalidRequest(message);
        let project_root = project_root.unwrap_or(self.cwd());
        let kind = settings::import::ImportSourceKind::try_parse(source).ok_or_else(|| {
            invalid(format!(
                "unknown source `{source}` — expected claude_code, codex, or cursor"
            ))
        })?;
        let sources = settings::import::detect_import_sources(project_root).await;
        let Some(matched) = sources.iter().find(|s| s.kind() == kind) else {
            return Err(invalid(format!(
                "source `{source}` not currently detected in this project"
            )));
        };
        let layout = self.layout();
        let summary = settings::import::execute_import(project_root, &layout, matched)
            .await
            .map_err(|e| invalid(e.to_string()))?;
        settings::import::mark_imported(project_root, &layout, &sources, kind)
            .await
            .map_err(|e| invalid(e.to_string()))?;
        if project_root == self.cwd() {
            self.events.import_answered();
        }
        Ok(api::ImportRun {
            source: summary.kind.as_str().to_string(),
            actions: summary.actions,
        })
    }
}
