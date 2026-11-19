//! The policy's read-only directories, held in place. A read-only mount
//! moves with the directory it is on when a directory above it is renamed,
//! so each directory between the writable root and a read-only one is bound
//! onto itself as well: a mount point cannot be renamed or removed, and what
//! else is in it stays writable.
//!
//! Every directory is held before anything is mounted read-only: binding a
//! directory onto itself covers what was mounted inside it before.

use super::super::roots::Roots;
use super::bind;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Laid right after the writable roots, under everything the project guard
/// mounts later.
pub(super) fn hold_read_only_dirs(args: &mut Vec<String>, roots: &Roots) {
    let dirs: BTreeSet<&Path> = roots.read_only_in_reach().filter(|d| d.is_dir()).collect();
    let above: BTreeSet<PathBuf> = dirs.iter().flat_map(|d| roots.movable_above(d)).collect();
    for dir in &above {
        bind(args, "--bind", dir);
    }
    for dir in dirs {
        bind(args, "--ro-bind", dir);
    }
}
