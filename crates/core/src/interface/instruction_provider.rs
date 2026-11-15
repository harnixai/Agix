//! `InstructionProvider` and `RuleProvider` — where the project's standing
//! instructions come from.
//!
//! Two things reach the model without anyone asking for them: the instruction
//! file (`AGENTS.md` / `CLAUDE.md`), injected verbatim, and the `.agix/rules/`
//! index, advertised by name and first line so the model knows the documents
//! exist. Both were a fixed answer to a question a deployment may answer
//! differently — one path out of `settings.instruction_file`, and three
//! directory tiers written into the discovery function. A host whose
//! conventions live in a service, a monorepo tool that composes its own tier
//! list, or an embedder with no filesystem at all had nothing to replace.
//!
//! The two stay separate contracts because they are read differently:
//! instruction documents are content that goes into the prompt, rules are an
//! index whose content is deliberately *not* loaded until the model asks for
//! it. The sources that read files (`InstructionFile`, `RuleDirectory`) live
//! in `prompt_files`; what is here is the contract and the in-memory ones.

use crate::rules::RuleEntry;
use std::path::PathBuf;

/// One standing-instruction document.
pub struct InstructionDoc {
    /// Where it came from, for the header the engine writes when more than
    /// one document applies. Need not exist on disk.
    pub path: PathBuf,
    pub content: String,
}

/// A place standing instructions come from.
pub trait InstructionProvider: Send + Sync {
    fn id(&self) -> &str;

    /// The documents that apply, outermost first — the engine treats the last
    /// one as the most specific and lets it win a conflict.
    fn documents(&self) -> Vec<InstructionDoc>;
}

/// Instructions the host already has in memory.
///
/// A daemon that fetched the project's conventions over its own API, or a
/// test harness that wants a known instruction text without a temp directory,
/// hands them over directly rather than staging a file for the engine to read
/// back.
pub struct InlineInstructions {
    label: PathBuf,
    text: String,
}

impl InlineInstructions {
    pub fn new(label: impl Into<PathBuf>, text: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            text: text.into(),
        }
    }
}

impl InstructionProvider for InlineInstructions {
    fn id(&self) -> &str {
        "inline"
    }

    fn documents(&self) -> Vec<InstructionDoc> {
        if self.text.trim().is_empty() {
            return Vec::new();
        }
        vec![InstructionDoc {
            path: self.label.clone(),
            content: self.text.clone(),
        }]
    }
}

/// A place rule documents come from.
///
/// Ordered by the caller: a later source's entry replaces an earlier one of
/// the same name, which is how project beats scene beats global.
pub trait RuleProvider: Send + Sync {
    fn id(&self) -> &str;

    fn rules(&self) -> Vec<RuleEntry>;
}

/// Rules the host already knows about.
///
/// The index is a name, a one-line summary and something the Read tool can
/// open — none of which has to come from a directory scan. A deployment
/// serving its rule documents out of a repository service builds the entries
/// itself and keeps the paths its own tooling understands.
pub struct StaticRules {
    id: String,
    entries: Vec<RuleEntry>,
}

impl StaticRules {
    pub fn new(id: impl Into<String>, entries: Vec<RuleEntry>) -> Self {
        Self {
            id: id.into(),
            entries,
        }
    }
}

impl RuleProvider for StaticRules {
    fn id(&self) -> &str {
        &self.id
    }

    fn rules(&self) -> Vec<RuleEntry> {
        self.entries.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_host_can_supply_instructions_without_a_file() {
        let docs = InlineInstructions::new("service://conventions", "Be brief.").documents();
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].content, "Be brief.");
        assert!(InlineInstructions::new("x", "  ").documents().is_empty());
    }

    #[test]
    fn a_host_can_supply_a_rule_index_it_built_itself() {
        let entries = vec![RuleEntry {
            name: "testing".into(),
            description: "How we test".into(),
            path: PathBuf::from("/srv/rules/testing.md"),
        }];
        let source = StaticRules::new("service", entries.clone());
        assert_eq!(source.id(), "service");
        assert_eq!(source.rules(), entries);
    }
}
