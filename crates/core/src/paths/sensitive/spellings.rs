//! A directory spelled the ways a path or a command may name it: as given,
//! as the filesystem resolves it, lowered component by component, and from
//! `$HOME` — what [`super::SensitivePaths`] matches paths and command text
//! against.

use std::path::{Component, Path, PathBuf};

/// A directory as given, normalized, and as the filesystem resolves it;
/// and both lowercased component by component.
#[derive(Debug, Clone)]
pub(super) struct Spellings {
    pub(super) given: PathBuf,
    pub(super) real: PathBuf,
    pub(super) lowered: Vec<Vec<String>>,
    /// The lowered components below `$HOME`, for each spelling under it:
    /// what follows `~/` or `$HOME/` in a command.
    pub(super) below_home: Vec<Vec<String>>,
}

impl Spellings {
    pub(super) fn of(dir: &Path) -> Self {
        Self::of_with_home(dir, crate::paths::home_dir().as_deref())
    }

    pub(super) fn of_with_home(dir: &Path, home: Option<&Path>) -> Self {
        let given = absolute_lexically(dir);
        let real = std::fs::canonicalize(&given).unwrap_or_else(|_| given.clone());
        let mut lowered = vec![lowered_components(&given)];
        let real_lowered = lowered_components(&real);
        if !lowered.contains(&real_lowered) {
            lowered.push(real_lowered);
        }
        let below_home = home
            .map(|home| lowered_components(&absolute_lexically(home)))
            .map(|home| {
                lowered
                    .iter()
                    .filter_map(|spelling| spelling.strip_prefix(home.as_slice()))
                    .filter(|rest| !rest.is_empty())
                    .map(<[String]>::to_vec)
                    .collect()
            })
            .unwrap_or_default();
        Self {
            given,
            real,
            lowered,
            below_home,
        }
    }

    pub(super) fn spell(&self, path: &Path) -> bool {
        path.starts_with(&self.given) || path.starts_with(&self.real)
    }
}

pub(super) fn absolute_lexically(path: &Path) -> PathBuf {
    crate::path::normalize_lexically(
        &std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf()),
    )
}

pub(super) fn lowered_components(path: &Path) -> Vec<String> {
    path.components()
        .filter_map(|c| match c {
            Component::Normal(name) => Some(crate::paths::fold_case(&name.to_string_lossy())),
            _ => None,
        })
        .collect()
}
