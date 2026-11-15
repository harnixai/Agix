//! The rule index: its entries and the merge of several rule sources.
//!
//! Rules stay lazy: the model sees each document's name and first line and
//! reads the rest on demand. Where the documents come from is the
//! [`RuleProvider`](crate::interface::instruction_provider::RuleProvider)
//! contract; the directory-backed sources are in `prompt_files`.

use crate::interface::instruction_provider::RuleProvider;
use std::path::PathBuf;
use std::sync::Arc;

/// One discovered rule document.
#[derive(Debug, Clone, PartialEq)]
pub struct RuleEntry {
    /// Filename stem (e.g. "security" for `security.md`).
    pub name: String,
    /// First non-empty line of the file, `#`/whitespace-stripped — same
    /// "fallback to first body line" convention as skills. Capped at
    /// `prompt_files::MAX_RULE_DESCRIPTION_CHARS` regardless of how verbose the line is.
    pub description: String,
    /// Absolute path — always resolvable by the Read tool regardless of
    /// which tier (project/scene/global) the file came from.
    pub path: PathBuf,
}

/// Rule sources merged into one index: a later source's entry replaces an
/// earlier one of the same name, and the result is sorted by name.
#[derive(Clone)]
pub struct RuleTiers {
    sources: Vec<Arc<dyn RuleProvider>>,
}

impl RuleTiers {
    pub fn new(sources: Vec<Arc<dyn RuleProvider>>) -> Self {
        Self { sources }
    }
}

impl RuleProvider for RuleTiers {
    fn id(&self) -> &str {
        "tiers"
    }

    fn rules(&self) -> Vec<RuleEntry> {
        let mut by_name: std::collections::HashMap<String, RuleEntry> =
            std::collections::HashMap::new();
        for source in &self.sources {
            for entry in source.rules() {
                by_name.insert(entry.name.clone(), entry);
            }
        }
        let mut entries: Vec<RuleEntry> = by_name.into_values().collect();
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        entries
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interface::instruction_provider::StaticRules;

    fn entry(name: &str, description: &str, path: &str) -> RuleEntry {
        RuleEntry {
            name: name.into(),
            description: description.into(),
            path: PathBuf::from(path),
        }
    }

    #[test]
    fn the_merge_is_sorted_by_name() {
        let tiers = RuleTiers::new(vec![Arc::new(StaticRules::new(
            "s",
            vec![entry("b", "", "/b.md"), entry("a", "", "/a.md")],
        ))]);
        let names: Vec<String> = tiers.rules().into_iter().map(|e| e.name).collect();
        assert_eq!(names, ["a", "b"]);
    }

    /// The tier list is a composition of sources, not three paths written
    /// into the discovery function — so a fourth source merges by the same
    /// last-wins rule the tiers use.
    #[test]
    fn a_source_the_engine_does_not_own_takes_part_in_the_merge() {
        let sources: Vec<Arc<dyn RuleProvider>> = vec![
            Arc::new(StaticRules::new(
                "service",
                vec![RuleEntry {
                    name: "testing".into(),
                    description: "from the service".into(),
                    path: PathBuf::from("/srv/testing.md"),
                }],
            )),
            Arc::new(StaticRules::new(
                "override",
                vec![RuleEntry {
                    name: "testing".into(),
                    description: "from the later source".into(),
                    path: PathBuf::from("/late/testing.md"),
                }],
            )),
        ];
        let entries = RuleTiers::new(sources).rules();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].description, "from the later source");
    }
}
