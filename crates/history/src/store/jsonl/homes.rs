//! Where one session's files are: the JSONL store's one place that answers
//! it. A session kept globally has its transcript under `projects/` and its
//! sidecar under `sessions/<id>/`; one kept in its project has both in the
//! project's `state/history/<id>/`, and only an index in `sessions/<id>/`.
//! Everything that reads or writes a session's files asks [`Homes::locate`].

use crate::entry::{LogEntry, SessionKind};
use crate::error::HistoryError;
use crate::path::{project_dir_for, session_file, session_metadata_file, session_sidecar_dir};
use crate::placement::{ProjectHistory, SessionIndex};
use crate::project::SessionMetadata;
use base::session::SessionId;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

pub(super) struct Homes {
    pub(super) projects_root: PathBuf,
    pub(super) sessions_root: PathBuf,
    /// The project a session with nothing recorded about it belongs to.
    pub(super) canonical_cwd: PathBuf,
    projects: Option<Arc<dyn ProjectHistory>>,
    known: Mutex<HashMap<SessionId, Home>>,
    /// Projects asked about in this process: where a session whose project
    /// has moved is looked for.
    opened: Mutex<Vec<PathBuf>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Home {
    /// The session's own project; `None` for a session with none.
    pub(super) project: Option<PathBuf>,
    pub(super) kept: Kept,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Kept {
    Global,
    InProject(SessionIndex),
}

/// Reading a session kept in a project needs the project trusted now;
/// writing to one already placed there does not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Access {
    Read,
    Write,
}

impl Home {
    pub(super) fn cwd(&self) -> &Path {
        self.project.as_deref().unwrap_or_else(|| Path::new(""))
    }

    pub(super) fn index(&self) -> Option<&SessionIndex> {
        match &self.kept {
            Kept::InProject(index) => Some(index),
            Kept::Global => None,
        }
    }

    /// The paths in `entry` as they are now, for a session whose project has
    /// moved since its log was written.
    pub(super) fn rebase(&self, entry: &mut LogEntry) {
        let Some(index) = self.index() else { return };
        let (from, to) = (index.recorded_root(), index.project_root.as_path());
        if same_place(from, to) {
            return;
        }
        if let LogEntry::Meta {
            cwd, project_root, ..
        } = entry
        {
            *cwd = moved(cwd, from, to);
            if let Some(root) = project_root {
                *root = moved(root, from, to);
            }
        }
    }
}

fn moved(path: &str, from: &Path, to: &Path) -> String {
    match Path::new(path).strip_prefix(from) {
        Ok(rest) if rest.as_os_str().is_empty() => to.display().to_string(),
        Ok(rest) => to.join(rest).display().to_string(),
        Err(_) => path.to_string(),
    }
}

pub(super) fn same_place(a: &Path, b: &Path) -> bool {
    a == b || matches!((a.canonicalize(), b.canonicalize()), (Ok(a), Ok(b)) if a == b)
}

fn project_of(metadata: &SessionMetadata) -> Option<PathBuf> {
    // An empty `canonical_cwd` is how a no-project session records itself.
    (!metadata.canonical_cwd.is_empty()).then(|| PathBuf::from(&metadata.canonical_cwd))
}

pub(super) fn read_metadata(sidecar_root: &Path, session: SessionId) -> Option<SessionMetadata> {
    let text =
        crate::own_file::read_to_string_blocking(&session_metadata_file(sidecar_root, &session))
            .ok()?;
    serde_json::from_str(&text).ok()
}

impl Homes {
    pub(super) fn new(
        projects_root: PathBuf,
        sessions_root: PathBuf,
        canonical_cwd: PathBuf,
    ) -> Self {
        Self {
            projects_root,
            sessions_root,
            canonical_cwd,
            projects: None,
            known: Mutex::default(),
            opened: Mutex::default(),
        }
    }

    pub(super) fn with_projects(self, projects: Arc<dyn ProjectHistory>) -> Self {
        Self {
            projects: Some(projects),
            ..self
        }
    }

    pub(super) fn projects(&self) -> Option<&Arc<dyn ProjectHistory>> {
        self.projects.as_ref()
    }

    fn known(&self) -> MutexGuard<'_, HashMap<SessionId, Home>> {
        self.known.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(super) fn remember(&self, session: SessionId, home: Home) {
        self.known().insert(session, home);
    }

    pub(super) fn forget(&self, session: SessionId) {
        self.known().remove(&session);
    }

    pub(super) fn note_project(&self, root: &Path) {
        let mut opened = self.opened.lock().unwrap_or_else(|e| e.into_inner());
        if !opened.iter().any(|p| p == root) {
            opened.push(root.to_path_buf());
        }
    }

    /// The directory the sidecar directories of `home`'s kind sit in.
    pub(super) fn sidecar_root<'a>(&'a self, home: &'a Home) -> &'a Path {
        match &home.kept {
            Kept::Global => &self.sessions_root,
            Kept::InProject(index) => index.history_dir(),
        }
    }

    pub(super) fn sidecar_dir(&self, home: &Home, session: SessionId) -> PathBuf {
        session_sidecar_dir(self.sidecar_root(home), &session)
    }

    pub(super) fn transcript_dir(&self, home: &Home) -> PathBuf {
        match &home.kept {
            Kept::Global => project_dir_for(&self.projects_root, home.project.as_deref()),
            Kept::InProject(index) => index.dir.clone(),
        }
    }

    pub(super) fn transcript(&self, home: &Home, session: SessionId) -> PathBuf {
        session_file(&self.transcript_dir(home), &session)
    }

    /// Where `session`'s files are.
    ///
    /// A session kept in a project is one its global index names, and only
    /// that: an id found only in a project's directory is not anyone's
    /// session. Reading it needs the project trusted, asked again every time
    /// so that taking trust back takes effect at once.
    ///
    /// A session nothing is recorded about is placed under the store's own
    /// project, which is where every session lived when a store served one
    /// project; that answer is not remembered, because the session's `Meta`
    /// may be about to record another.
    pub(super) fn locate(&self, session: SessionId, access: Access) -> Result<Home, HistoryError> {
        let cached = self.known().get(&session).cloned();
        let home = match cached {
            Some(home) => home,
            None => self.find(session)?,
        };
        if access == Access::Read {
            self.check_trust(session, &home)?;
        }
        Ok(home)
    }

    fn find(&self, session: SessionId) -> Result<Home, HistoryError> {
        if let Some(index) = SessionIndex::read(&self.sessions_root, session)? {
            self.check_shape(session, &index)?;
            let index = self.follow_move(session, index);
            let project = match read_metadata(index.history_dir(), session) {
                Some(metadata) => project_of(&metadata),
                None => Some(index.project_root.clone()),
            };
            let home = Home {
                project,
                kept: Kept::InProject(index),
            };
            self.remember(session, home.clone());
            return Ok(home);
        }
        match read_metadata(&self.sessions_root, session) {
            Some(metadata) => {
                let home = Home {
                    project: project_of(&metadata),
                    kept: Kept::Global,
                };
                self.remember(session, home.clone());
                Ok(home)
            }
            None => Ok(Home {
                project: Some(self.canonical_cwd.clone()),
                kept: Kept::Global,
            }),
        }
    }

    pub(super) fn check_trust(&self, session: SessionId, home: &Home) -> Result<(), HistoryError> {
        let Some(index) = home.index() else {
            return Ok(());
        };
        match &self.projects {
            Some(projects) if projects.trusted(&index.project_root) => Ok(()),
            _ => Err(HistoryError::ProjectUntrusted {
                session: session.to_string(),
                project: index.project_root.clone(),
            }),
        }
    }

    /// An index is taken only when its `dir` is the session's directory in
    /// its project's history directory.
    fn check_shape(&self, session: SessionId, index: &SessionIndex) -> Result<(), HistoryError> {
        let Some(projects) = &self.projects else {
            return Ok(());
        };
        let expected = projects
            .history_of(&index.project_root)
            .map(|found| found.dir.join(session.to_string()));
        if expected.as_ref() == Some(&index.dir) {
            return Ok(());
        }
        Err(HistoryError::Path(format!(
            "the index of session {session} puts it in {}, which is not its project's history directory",
            index.dir.display()
        )))
    }

    /// `index`, pointed at where its project is now when the project it
    /// names is gone and a project opened here holds the session — a trusted
    /// one, and only for an id the index already has.
    ///
    /// "Gone" is the project root missing from a directory that is there: a
    /// volume not mounted, a network share not reachable or a directory not
    /// readable says nothing about where the project is, and the index stays.
    fn follow_move(&self, session: SessionId, index: SessionIndex) -> SessionIndex {
        if !not_found(&index.dir) || !gone_from_its_parent(&index.project_root) {
            return index;
        }
        let Some(projects) = &self.projects else {
            return index;
        };
        let candidates: Vec<PathBuf> = std::iter::once(self.canonical_cwd.clone())
            .chain(
                self.opened
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone(),
            )
            .collect();
        for candidate in candidates {
            let Some(found) = projects.history_of(&candidate) else {
                continue;
            };
            let dir = found.dir.join(session.to_string());
            if !session_file(&dir, &session).is_file() || !projects.trusted(&found.root) {
                continue;
            }
            let moved_to = SessionIndex {
                project_root: found.root.clone(),
                dir,
                recorded_root: Some(index.recorded_root().to_path_buf()),
                ..index.clone()
            };
            if let Err(e) = moved_to.write(&self.sessions_root) {
                tracing::warn!(%session, error = %e, "the session's project moved, but its index could not be rewritten");
            }
            rewrite_metadata(session, &index.project_root, &moved_to);
            tracing::info!(%session, from = %index.project_root.display(), to = %moved_to.project_root.display(), "the session's project moved; its index follows it");
            return moved_to;
        }
        index
    }

    /// Where a session being created now goes: where its parent is, for a
    /// sidechain or fork of the same project, so that a lineage stays in one
    /// directory; otherwise — a fork imported into another project too — its
    /// project when the host keeps new sessions there.
    /// `recorded` is the project root as the session's `Meta` spells it,
    /// which a move is later read against.
    ///
    /// The session's directory in the project is made here and must not be
    /// there yet: one that is was put there by someone else — a repository,
    /// another store with its own global root — and this session is kept
    /// globally instead of taking in what is in it.
    pub(super) fn place(
        &self,
        session: SessionId,
        project: Option<&Path>,
        recorded: Option<&Path>,
        parent: Option<SessionId>,
        kind: SessionKind,
    ) -> Kept {
        let Kept::InProject(mut index) = self.place_at(session, project, recorded, parent) else {
            return Kept::Global;
        };
        index.parent_session_id = parent;
        index.session_kind = kind;
        match make_own_dir(&index) {
            Ok(()) => Kept::InProject(index),
            Err(e) => {
                tracing::warn!(%session, dir = %index.dir.display(), error = %e, "the session's directory in its project could not be made its own; the session is kept globally");
                Kept::Global
            }
        }
    }

    fn place_at(
        &self,
        session: SessionId,
        project: Option<&Path>,
        recorded: Option<&Path>,
        parent: Option<SessionId>,
    ) -> Kept {
        let Some(projects) = &self.projects else {
            return Kept::Global;
        };
        let of_parent = match parent {
            None => None,
            Some(parent) => match self.locate(parent, Access::Write) {
                Ok(Home {
                    kept: Kept::InProject(of_parent),
                    ..
                }) => Some(of_parent),
                _ => return Kept::Global,
            },
        };
        let in_parents_project = |of_parent: &SessionIndex| {
            project
                .and_then(|root| projects.history_of(root))
                .is_some_and(|found| found.root == of_parent.project_root)
        };
        let mut index = match of_parent.filter(in_parents_project) {
            Some(of_parent) => SessionIndex::new(
                session,
                of_parent.project_root.clone(),
                of_parent.history_dir(),
            ),
            None => match project.filter(|root| projects.keeps_new(root)) {
                Some(root) => match projects.history_of(root) {
                    Some(found) => SessionIndex::new(session, found.root, &found.dir),
                    None => return Kept::Global,
                },
                None => return Kept::Global,
            },
        };
        index.recorded_root = recorded
            .and_then(|spelled| projects.history_of(spelled))
            .map(|found| found.root)
            .filter(|spelled| *spelled != index.project_root);
        Kept::InProject(index)
    }

    /// The sessions kept in `history_dir` that their index puts there, each
    /// with its home. A directory there that no index names, or whose index
    /// names somewhere else, is left out; so is all of it while its project
    /// is not trusted.
    pub(super) fn indexed_in(&self, history_dir: &Path, access: Access) -> Vec<(SessionId, Home)> {
        let Ok(entries) = std::fs::read_dir(history_dir) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for entry in entries.flatten() {
            let Some(session) = entry
                .file_name()
                .to_str()
                .and_then(|name| SessionId::parse(name).ok())
            else {
                continue;
            };
            let Ok(home) = self.locate(session, access) else {
                continue;
            };
            if home
                .index()
                .is_some_and(|index| same_place(&index.dir, &entry.path()))
            {
                out.push((session, home));
            }
        }
        out
    }

    /// Every session the global index places in a project, with its home;
    /// those in projects not trusted now are left out.
    pub(super) fn all_indexed(&self) -> Vec<(SessionId, Home)> {
        let Ok(entries) = std::fs::read_dir(&self.sessions_root) else {
            return Vec::new();
        };
        entries
            .flatten()
            .filter_map(|entry| SessionId::parse(entry.file_name().to_str()?).ok())
            .filter_map(|session| {
                let home = self.locate(session, Access::Read).ok()?;
                home.index().is_some().then_some((session, home))
            })
            .collect()
    }
}

fn not_found(path: &Path) -> bool {
    matches!(path.symlink_metadata(), Err(e) if e.kind() == std::io::ErrorKind::NotFound)
}

fn gone_from_its_parent(path: &Path) -> bool {
    not_found(path) && path.parent().is_some_and(Path::is_dir)
}

fn make_own_dir(index: &SessionIndex) -> std::io::Result<()> {
    let history = index.history_dir();
    std::fs::create_dir_all(history)?;
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(&index.dir)?;
    #[cfg(unix)]
    std::fs::File::open(history)?.sync_all()?;
    Ok(())
}

fn rewrite_metadata(session: SessionId, from: &Path, index: &SessionIndex) {
    let root = index.history_dir();
    let Some(mut metadata) = read_metadata(root, session) else {
        return;
    };
    metadata.canonical_cwd = moved(&metadata.canonical_cwd, from, &index.project_root);
    metadata.history_file = session_file(&index.dir, &session).display().to_string();
    let written = serde_json::to_vec_pretty(&metadata)
        .map_err(std::io::Error::other)
        .and_then(|bytes| {
            base::fs::write_atomic(
                &session_metadata_file(root, &session),
                &bytes,
                base::fs::WriteOpts::DURABLE,
            )
        });
    if let Err(e) = written {
        tracing::warn!(%session, error = %e, "the moved session's metadata could not be rewritten");
    }
}
