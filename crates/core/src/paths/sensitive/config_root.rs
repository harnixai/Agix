//! A configuration root placed in a directory, as the sandbox backends guard
//! it: shut to writes, or — the instance's own — open only in the working
//! entries of its `state/`, the same ones the gate leaves out of
//! configuration.

use std::path::PathBuf;

use super::is_working_entry;
use crate::paths::layout::{entry, STATE_ROOT_WORKING_DIRS};

/// A configuration root in a directory, as [`super::SensitivePaths::config_roots_in`]
/// gives it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigRootAt {
    pub path: PathBuf,
    /// `path` with its symbolic links resolved.
    pub resolved: PathBuf,
    /// The instance's own project directory: held even when absent, the
    /// working entries of its `state/` left writable.
    pub own: bool,
}

impl ConfigRootAt {
    /// Whether a sandboxed command may write nothing in this root but what
    /// the policy itself names writable: a foreign root, every entry of
    /// which the gate takes for configuration
    /// ([`super::SensitivePaths::names_configuration`]).
    pub fn shut_to_writes(&self) -> bool {
        !self.own
    }

    /// `<root>/state`, as given.
    pub fn state_dir(&self) -> PathBuf {
        self.path.join(entry::STATE)
    }

    /// Whether a sandboxed command may write the entry `name` directly in
    /// this root's `state/`: in the instance's own, its working space
    /// ([`super::SensitivePaths::is_engine_working_entry`]), which is all the gate
    /// leaves out of configuration there; in a foreign one none
    /// ([`Self::shut_to_writes`]). Nothing directly in the root itself, nor
    /// `state/` as a whole.
    pub fn writable_state_entry(&self, name: &str) -> bool {
        !self.shut_to_writes() && is_working_entry(name)
    }

    /// The working entries of this root's `state/` as the root resolves,
    /// whether or not they exist: what [`Self::writable_state_entry`] admits.
    pub fn working_entries(&self) -> Vec<PathBuf> {
        STATE_ROOT_WORKING_DIRS
            .iter()
            .filter(|name| self.writable_state_entry(name))
            .map(|name| self.resolved.join(entry::STATE).join(name))
            .collect()
    }
}
