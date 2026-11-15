//! `session.import` on the pool: read and check the archive, place it, write
//! the sessions, and take them back out if one cannot be written.

use crate::import::{
    check_source, id_map, read_archive, rewrite, Archive, ImportRequest, ProjectRootParam, Target,
};
use crate::session_pool::*;
use base::session::SessionId;
use history::entry::{EnvelopedEntry, LogEntry};
use history::store::HistoryStore;
use std::collections::HashMap;

impl SessionPool {
    /// `session.import` — an archive written by `session.export`, as new
    /// sessions (see `crate::import`). Everything is checked before the first
    /// line is written; a failure while writing removes what was written. Like
    /// a fork, the sessions are on disk and not live.
    pub async fn import_session(
        &self,
        request: ImportRequest,
    ) -> Result<api::ImportSummary, PoolError> {
        let store = self.directory.require_store()?.clone();
        let settings = self.bindings.pool_settings().await;
        let roots = history::path::HistoryRoots::under(&settings.paths.global_data_dir);
        check_source(
            &request.path,
            &[roots.projects.clone(), roots.sessions.clone()],
        )?;
        let source = request.path.clone();
        let archive = tokio::task::spawn_blocking(move || read_archive(&source))
            .await
            .map_err(|e| PoolError::Internal(format!("the import task failed: {e}")))??;
        let target = self.placement(
            &archive,
            &request.project_root,
            &settings.paths.global_data_dir,
        )?;

        let ids = id_map(&archive, request.keep_ids);
        let _reservation = if request.keep_ids {
            let reservation = self.directory.reserve_for_import(ids.values().copied())?;
            refuse_existing(store.as_ref(), ids.values().copied()).await?;
            Some(reservation)
        } else {
            None
        };
        let mut warnings = Vec::new();
        let mut written = rewrite(&archive, &ids, &target, &mut warnings);
        for (_, entries) in &mut written {
            entries.retain(|env| carried_by_an_import(&env.entry));
        }
        let old_of: HashMap<SessionId, SessionId> =
            ids.iter().map(|(old, new)| (*new, *old)).collect();
        warn_of_count_mismatch(&archive, &old_of, &written, store.as_ref(), &mut warnings);

        let summary = write_sessions(store.as_ref(), written).await?;
        for session in &archive.sessions {
            let Some(memory) = &session.memory else {
                continue;
            };
            let file = history::path::session_memory_file(&roots.sessions, &ids[&session.id]);
            if let Err(e) = write_file(&file, memory).await {
                warnings.push(format!(
                    "the session memory of {} could not be written: {e}",
                    session.id
                ));
            }
        }
        info!(root = %ids[&archive.root], sessions = summary.len(), "session imported");
        Ok(api::ImportSummary {
            root_session_id: ids[&archive.root].to_string(),
            id_map: ids
                .iter()
                .map(|(old, new)| (old.to_string(), new.to_string()))
                .collect(),
            sessions: summary,
            warnings,
        })
    }

    fn placement(
        &self,
        archive: &Archive,
        asked: &ProjectRootParam,
        global_data_dir: &Path,
    ) -> Result<Target, PoolError> {
        let Some(LogEntry::Meta {
            scene,
            project_root: recorded,
            ..
        }) = archive.meta_of(archive.root)
        else {
            return Err(PoolError::InvalidRequest(
                "the root session has no Meta line".into(),
            ));
        };
        if let Some(scene) = scene
            .as_deref()
            .filter(|s| self.scenes.resolve(Some(s)).is_none())
        {
            return Err(PoolError::InvalidRequest(format!(
                "the archive's scene `{scene}` is not available on this daemon"
            )));
        }
        let project_root = match asked {
            ProjectRootParam::Omitted => recorded.clone(),
            ProjectRootParam::Null => None,
            ProjectRootParam::Path(path) => Some(path.display().to_string()),
        };
        if project_root
            .as_deref()
            .is_some_and(|p| !Path::new(p).is_dir())
        {
            return Err(PoolError::InvalidRequest(format!(
                "project_root `{}` does not exist or is not a directory; pass project_root \
                 to place the imported sessions under another one",
                project_root.unwrap_or_default()
            )));
        }
        Ok(Target {
            cwd: project_root
                .clone()
                .unwrap_or_else(|| global_data_dir.display().to_string()),
            project_root,
        })
    }
}

/// An archive is somebody's data, not this daemon's record of what it did.
/// Its checkpoint records would have a rewind delete or put back whatever
/// files they name, and its recorded spec would choose the imported session's
/// prompt, permission rules and telemetry file on its first resume; neither
/// is carried. The options of the call that resumes it apply instead.
fn carried_by_an_import(entry: &LogEntry) -> bool {
    let checkpoint = matches!(entry, LogEntry::Extension { ns, .. }
        if ns == history::checkpoint::CHECKPOINT_NS);
    !(checkpoint || crate::session_pool::parts::specs::is_spec(entry))
}

async fn refuse_existing(
    store: &dyn HistoryStore,
    ids: impl Iterator<Item = SessionId>,
) -> Result<(), PoolError> {
    for id in ids {
        match store.load(id).await {
            Ok(_) => {
                return Err(PoolError::SessionExists {
                    session_id: id.to_string(),
                })
            }
            Err(history::error::HistoryError::SessionNotFound(_)) => {}
            Err(e) => {
                return Err(PoolError::Internal(format!(
                    "failed to look up a session: {e}"
                )))
            }
        }
    }
    Ok(())
}

fn warn_of_count_mismatch(
    archive: &Archive,
    old_of: &HashMap<SessionId, SessionId>,
    written: &[(SessionId, Vec<EnvelopedEntry>)],
    store: &dyn HistoryStore,
    warnings: &mut Vec<String>,
) {
    for (new, entries) in written {
        let recorded = old_of
            .get(new)
            .and_then(|old| archive.sessions.iter().find(|s| s.id == *old))
            .and_then(|s| s.message_count);
        let projected =
            history::transcript::project_messages_with(entries, store.projection()).len();
        if let Some(recorded) = recorded.filter(|n| *n != projected as u64) {
            warnings.push(format!(
                "session {new} projects {projected} messages; the archive recorded {recorded}"
            ));
        }
    }
}

async fn write_sessions(
    store: &dyn HistoryStore,
    written: Vec<(SessionId, Vec<EnvelopedEntry>)>,
) -> Result<Vec<api::ImportedSession>, PoolError> {
    let mut summary = Vec::with_capacity(written.len());
    let mut done = Vec::with_capacity(written.len());
    for (new, entries) in written {
        let (kind, parent) = match entries.first().map(|e| &e.entry) {
            Some(LogEntry::Meta {
                session_kind,
                parent_session_id,
                ..
            }) => ((*session_kind).into(), parent_session_id.clone()),
            _ => (api::SessionKindName::Primary, None),
        };
        summary.push(api::ImportedSession {
            session_id: new.to_string(),
            kind,
            parent_session_id: parent,
            entry_count: entries.len(),
        });
        done.push(new);
        let wrote = match store.append_enveloped(new, entries).await {
            Ok(()) => store.sync(new).await,
            Err(e) => Err(e),
        };
        if let Err(e) = wrote {
            for session in &done {
                let _ = store.delete(*session).await;
            }
            return Err(PoolError::Internal(format!(
                "failed to write an imported session: {e}"
            )));
        }
    }
    Ok(summary)
}

async fn write_file(file: &Path, text: &str) -> std::io::Result<()> {
    if let Some(dir) = file.parent() {
        tokio::fs::create_dir_all(dir).await?;
    }
    tokio::fs::write(file, text).await
}
