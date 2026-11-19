//! The configured instruction files (`AGENTS.md` / `CLAUDE.md`,
//! `prompt.instruction_files`) as an `InstructionProvider` that reads them
//! from disk, in order.

use base::interface::instruction_provider::{InstructionDoc, InstructionProvider};
use base::paths::PromptFiles;
use std::path::PathBuf;

pub struct InstructionFile {
    paths: Vec<PathBuf>,
    files: PromptFiles,
}

impl InstructionFile {
    pub fn new(paths: Vec<PathBuf>) -> Self {
        Self {
            paths,
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
        self.paths
            .iter()
            .filter_map(|path| match self.files.read_to_string(path) {
                Ok(content) if !content.trim().is_empty() => Some(InstructionDoc {
                    path: path.clone(),
                    content,
                }),
                Ok(_) => None,
                Err(e) => {
                    tracing::warn!(path = %path.display(), error = %e, "failed to read instruction file for CLAUDE.md context injection");
                    None
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn an_absent_instruction_file_contributes_nothing() {
        assert!(InstructionFile::new(Vec::new()).documents().is_empty());
        assert!(
            InstructionFile::new(vec![PathBuf::from("/nonexistent/AGENTS.md")])
                .documents()
                .is_empty()
        );
    }

    #[test]
    fn a_whitespace_only_instruction_file_contributes_nothing() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("AGENTS.md");
        std::fs::write(&path, "   \n\n").unwrap();
        assert!(InstructionFile::new(vec![path]).documents().is_empty());
    }

    #[test]
    fn the_file_source_hands_back_the_text_untouched() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("AGENTS.md");
        std::fs::write(&path, "# Conventions\n\nBe brief.\n").unwrap();
        let docs = InstructionFile::new(vec![path.clone()]).documents();
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].path, path);
        assert_eq!(docs[0].content, "# Conventions\n\nBe brief.\n");
    }

    #[test]
    fn several_files_are_read_in_the_order_given() {
        let dir = TempDir::new().unwrap();
        let (a, b) = (dir.path().join("a.md"), dir.path().join("b.md"));
        std::fs::write(&a, "A").unwrap();
        std::fs::write(&b, "B").unwrap();
        let missing = dir.path().join("missing.md");
        let docs = InstructionFile::new(vec![b.clone(), missing, a.clone()]).documents();
        let read: Vec<_> = docs
            .iter()
            .map(|d| (d.path.clone(), d.content.as_str()))
            .collect();
        assert_eq!(read, [(b, "B"), (a, "A")]);
    }
}
