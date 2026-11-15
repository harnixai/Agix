//! The configured instruction file (`AGENTS.md` / `CLAUDE.md`) as an
//! `InstructionProvider` that reads it from disk.

use base::interface::instruction_provider::{InstructionDoc, InstructionProvider};
use base::paths::PromptFiles;
use std::path::PathBuf;

pub struct InstructionFile {
    path: Option<PathBuf>,
    files: PromptFiles,
}

impl InstructionFile {
    pub fn new(path: Option<PathBuf>) -> Self {
        Self {
            path,
            files: PromptFiles::unscoped(),
        }
    }

    pub fn reading_with(mut self, files: PromptFiles) -> Self {
        self.files = files;
        self
    }
}

impl InstructionProvider for InstructionFile {
    fn id(&self) -> &str {
        "instruction_file"
    }

    fn documents(&self) -> Vec<InstructionDoc> {
        let Some(ref path) = self.path else {
            return Vec::new();
        };
        match self.files.read_to_string(path) {
            Ok(content) if !content.trim().is_empty() => vec![InstructionDoc {
                path: path.clone(),
                content,
            }],
            Ok(_) => Vec::new(),
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "failed to read instruction file for CLAUDE.md context injection");
                Vec::new()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn an_absent_instruction_file_contributes_nothing() {
        assert!(InstructionFile::new(None).documents().is_empty());
        assert!(
            InstructionFile::new(Some(PathBuf::from("/nonexistent/AGENTS.md")))
                .documents()
                .is_empty()
        );
    }

    #[test]
    fn a_whitespace_only_instruction_file_contributes_nothing() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("AGENTS.md");
        std::fs::write(&path, "   \n\n").unwrap();
        assert!(InstructionFile::new(Some(path)).documents().is_empty());
    }

    #[test]
    fn the_file_source_hands_back_the_text_untouched() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("AGENTS.md");
        std::fs::write(&path, "# Conventions\n\nBe brief.\n").unwrap();
        let docs = InstructionFile::new(Some(path.clone())).documents();
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].path, path);
        assert_eq!(docs[0].content, "# Conventions\n\nBe brief.\n");
    }
}
