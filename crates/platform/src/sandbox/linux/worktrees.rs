//! The Linux shape of the project guard for linked worktrees: their entries
//! under a git directory's `worktrees/`, the engines' `worktrees/` in the
//! project, and what of the git directory a command in one may write.

use super::super::roots::{subdirs, Roots, GIT_OBJECTS_INFO_DIR};
use super::{bind, bind_over, read_only};
use base::paths::{
    WorktreeGit, GIT_WORKTREES_DIR, GIT_WORKTREE_CONTROL_FILES, REPOSITORY_CONFIG_DIR,
};
use std::path::Path;

/// What a command in the linked worktree may write of its git directory
/// beyond the binds of `additional_writable`: its branch ref, and none of
/// `objects/info/`.
pub(super) fn guard_linked(args: &mut Vec<String>, roots: &Roots, linked: &WorktreeGit) {
    open_branch_ref(args, roots, linked);
    let objects_info = linked.dir.join(GIT_OBJECTS_INFO_DIR);
    if roots.opens(&objects_info) {
        read_only(args, &objects_info);
    }
}

/// The entries under `repo/worktrees/`: their control files read-only, and
/// where `repo` is `open`, the directory itself held as well (see
/// [`hold_worktree_parent`]), made first when `make` says so.
pub(super) fn guard_entries(
    args: &mut Vec<String>,
    roots: &Roots,
    repo: &Path,
    open: bool,
    make: bool,
) {
    let admin = repo.join(GIT_WORKTREES_DIR);
    let held = open && !roots.only_additional && hold_worktree_parent(args, &admin, make);
    for worktree in subdirs(&admin) {
        if held {
            bind(args, "--bind", &worktree);
        }
        let linked_entry = roots
            .worktree_git
            .as_ref()
            .is_some_and(|linked| linked.admin == worktree && roots.opens(&worktree));
        for f in GIT_WORKTREE_CONTROL_FILES {
            let file = worktree.join(f);
            // The linked worktree's own entry is writable: an absent control
            // file there is shut as an empty one.
            if linked_entry && file.symlink_metadata().is_err() {
                bind_over(args, Path::new("/dev/null"), &file);
            } else {
                bind(args, "--ro-bind-try", &file);
            }
        }
    }
}

/// The `.git` links of the engines' worktrees, read-only, and where the
/// project is open, each engine directory's `worktrees/` held around them.
pub(super) fn guard_links(args: &mut Vec<String>, roots: &Roots) {
    for links in roots.paths.worktree_dirs_in(&roots.cwd) {
        let held = roots.opens(&links)
            && !roots.only_additional
            && hold_worktree_parent(args, &links, false);
        for worktree in subdirs(&links) {
            if held {
                bind(args, "--bind", &worktree);
            }
            bind(args, "--ro-bind-try", &worktree.join(REPOSITORY_CONFIG_DIR));
        }
    }
}

/// The worktree's branch ref, where the policy lets it be written. git
/// rewrites it by renaming `<ref>.lock` over it, which a bind of the file
/// would refuse, so the directory it is in is bound writable instead and
/// every other entry there, as it is now, read-only again.
fn open_branch_ref(args: &mut Vec<String>, roots: &Roots, linked: &WorktreeGit) {
    let own = [linked.branch_ref(), linked.branch_ref_lock()];
    let Some(dir) = own[0].parent().filter(|dir| dir.is_dir()) else {
        return;
    };
    if !roots.opens(&own[0]) {
        return;
    }
    bind(args, "--bind", dir);
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten().filter(|e| !own.contains(&e.path())) {
        bind(args, "--ro-bind", &entry.path());
    }
}

/// `dir`, where linked worktrees are added, read-only, so that one added
/// after the command starts — which no bind below names — cannot be written
/// by what the command leaves running; the existing ones are bound writable
/// again by the caller. An absent `dir` is made first, empty, as git would
/// make it for the first worktree, when `make` says so. Whether it is held.
///
/// Not for the engine's own git (`only_additional_writable`), which adds
/// worktrees; a command's `git worktree add` fails under it.
fn hold_worktree_parent(args: &mut Vec<String>, dir: &Path, make: bool) -> bool {
    if make && dir.symlink_metadata().is_err() && std::fs::create_dir(dir).is_err() {
        return false;
    }
    if !dir.symlink_metadata().is_ok_and(|m| m.is_dir()) {
        return false;
    }
    bind(args, "--ro-bind", dir);
    true
}
