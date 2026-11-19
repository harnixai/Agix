//! Directories a command may write around but must not replace: the policy's
//! read-only directories, and the configuration inside a writable root the
//! policy names in a configuration root. A read-only mount moves with the
//! directory it is on when a directory above it is renamed, so each
//! directory between the writable root and the guarded one is bound onto
//! itself as well: a mount point cannot be renamed or removed, and what else
//! is in it stays writable.
//!
//! Every directory is held before anything is mounted read-only: binding a
//! directory onto itself covers what was mounted inside it before.

use super::super::roots::Roots;
use super::{bind, hide, read_only, shut_empty};
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

/// The configuration of each configuration root
/// ([`Roots::configuration_in`]) that lies in a writable root the policy
/// names inside it, held read-only there, and each configuration directory
/// a named root lies in read-only over it. Each credential below what this
/// mounts is hidden again. A named root that is a configuration root
/// `masked` is not bound back writable at all.
pub(super) fn hold_configuration_in_named_roots(
    args: &mut Vec<String>,
    roots: &Roots,
    masked: &[PathBuf],
    made: &mut Vec<PathBuf>,
) {
    let mut plan = Plan::default();
    for root in roots.paths.config_roots_in(&roots.cwd) {
        let named: Vec<&PathBuf> = roots
            .writable
            .iter()
            .filter(|w| w.starts_with(&root.resolved))
            .filter(|w| **w != root.resolved || !masked.contains(&root.path))
            .collect();
        let configuration = roots.configuration_in(&root.resolved);
        for top in &named {
            for dir in configuration.dirs.iter().filter(|d| inside(d, top)) {
                if plan.hold_between(top, dir) {
                    plan.dirs.insert(dir.clone());
                }
            }
            for file in configuration.files.iter().filter(|f| inside(f, top)) {
                if plan.hold_between(top, file) {
                    plan.files.insert(file.clone());
                }
            }
        }
        plan.over_named.extend(
            configuration
                .dirs
                .into_iter()
                .filter(|d| named.iter().any(|w| w.starts_with(d))),
        );
    }
    for dir in &plan.held {
        bind(args, "--bind", dir);
    }
    for dir in plan.missing.iter().chain(&plan.dirs) {
        shut_empty(args, dir, made);
    }
    // An absent file cannot be held: its mount point would be left behind
    // as an empty configuration file.
    for file in &plan.files {
        bind(args, "--ro-bind-try", file);
    }
    for dir in &plan.over_named {
        read_only(args, dir);
    }
    let secrets = roots
        .paths
        .credential_locations(&roots.paths.engine_dirs_in(&roots.cwd));
    let mounted: Vec<&PathBuf> = plan.dirs.iter().chain(&plan.over_named).collect();
    for secret in secrets
        .iter()
        .flat_map(|l| l.existing())
        .filter(|s| mounted.iter().any(|dir| s.starts_with(dir)))
    {
        hide(args, &secret);
    }
}

#[derive(Default)]
struct Plan {
    held: BTreeSet<PathBuf>,
    missing: BTreeSet<PathBuf>,
    dirs: BTreeSet<PathBuf>,
    files: BTreeSet<PathBuf>,
    over_named: BTreeSet<PathBuf>,
}

impl Plan {
    /// Hold each directory strictly between `top` and `path`. Where one is
    /// missing, the topmost missing one is shut empty instead, so that the
    /// way down cannot be made; where one is not a real directory, nothing
    /// is. Whether the way down is held.
    fn hold_between(&mut self, top: &Path, path: &Path) -> bool {
        let between: Vec<&Path> = path
            .ancestors()
            .skip(1)
            .take_while(|a| inside(a, top))
            .collect();
        if let Some(missing) = between.iter().rev().find(|a| a.symlink_metadata().is_err()) {
            self.missing.insert(missing.to_path_buf());
            return false;
        }
        if !between
            .iter()
            .all(|a| a.symlink_metadata().is_ok_and(|m| m.is_dir()))
        {
            return false;
        }
        self.held.extend(between.iter().map(|a| a.to_path_buf()));
        true
    }
}

fn inside(path: &Path, top: &Path) -> bool {
    path.starts_with(top) && path != top
}
