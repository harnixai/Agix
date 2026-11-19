//! `session.create` 带调用方给的 `session_id`：同一个 id 只建一次。已有的会话（内存里的，
//! 或存储里有日志的，sidechain 也算）场景、项目与建它时的 `options` 都一致就答它，
//! 不一致就 `SESSION_EXISTS`。

use super::create::{created_answer, NewPlace};
use crate::session_pool::api::RecorderOptions;
use crate::session_pool::parts::directory::find_meta;
use crate::session_pool::parts::specs::recorded_in;
use crate::session_pool::*;

/// What already holds a given id.
struct Holder {
    scene: String,
    project: Option<PathBuf>,
    kind: history::entry::SessionKind,
    binding: Arc<ProjectBinding>,
    /// The options it was created with, and whether this process holds them
    /// (with a replay it was given) or only its log does.
    created_with: Option<SessionOptions>,
    held_here: bool,
    /// Its log, for one not in memory.
    log: Option<Vec<history::entry::EnvelopedEntry>>,
}

impl SessionPool {
    /// Create the session `sid` at `place`, unless `sid` already is a
    /// session. One that is, in `place`'s scene and project and created with
    /// these `options`, is the answer, brought into memory as
    /// `session.resume` would; any other is `SESSION_EXISTS`, nothing
    /// changed.
    ///
    /// All of it runs under `sid`'s session lock, which a turn creating `sid`
    /// on a miss and a delete take too, and with the id reserved against an
    /// import keeping ids: of two callers asking for one id, one creates it
    /// and the other finds it, and a session found is not deleted before it
    /// is answered.
    pub(in crate::session_pool) async fn create_or_find(
        &self,
        sid: base::session::SessionId,
        place: NewPlace,
        options: Option<&SessionOptions>,
    ) -> Result<api::SessionCreated, PoolError> {
        let id = sid.to_string();
        self.live
            .with_session_lock(&id, async {
                let _reserved = self
                    .directory
                    .reserve_for_import(std::iter::once(sid))
                    .map_err(|_| PoolError::SessionBusy(api::Busy::importing(&id)))?;
                let Some(holder) = self.holder_of(&id).await? else {
                    return self.start_new_session(id.clone(), &place, options).await;
                };
                holder.refuse_unless_asked_for(&id, &place, options)?;
                let core = match &holder.log {
                    None => self.live.core_of(&id).await,
                    Some(log) => Some(self.bring_back_locked(&id, options, log).await?.0),
                }
                .ok_or_else(|| PoolError::session_not_found(&id))?;
                Ok(created_answer(
                    &core,
                    &holder.scene,
                    holder.project.as_deref(),
                    &holder.binding,
                    options,
                    true,
                ))
            })
            .await
    }

    /// The session already under `sid`: in memory, or with a log in the
    /// store. A log that is there but does not read is an error, never an id
    /// free to take.
    async fn holder_of(&self, sid: &str) -> Result<Option<Holder>, PoolError> {
        if let Some(core) = self.live.core_of(sid).await {
            let scene = core.scene().id.clone();
            let project = core.project().map(Path::to_path_buf);
            return Ok(Some(Holder {
                kind: self.directory.facts_of(sid).await.session_kind,
                binding: self.binding_for(project.as_deref(), &scene).await,
                scene,
                project,
                created_with: self.specs.get(sid).and_then(|spec| spec.options),
                held_here: true,
                log: None,
            }));
        }
        self.live
            .wait_until_stopped(sid)
            .await
            .map_err(PoolError::from)?;
        let log = self.directory.read_log(sid).await?;
        if log.is_empty() {
            return Ok(None);
        }
        let kind = match find_meta(&log) {
            Some(history::entry::LogEntry::Meta { session_kind, .. }) => *session_kind,
            _ => history::entry::SessionKind::Primary,
        };
        let (binding, project, scene) = self.binding_of_log(&log).await;
        let (created_with, held_here) = match self.specs.get(sid) {
            Some(kept) => (kept.options, true),
            None => (recorded_in(&log).options, false),
        };
        Ok(Some(Holder {
            scene,
            project,
            kind,
            binding,
            created_with,
            held_here,
            log: Some(log),
        }))
    }
}

impl Holder {
    /// `SESSION_EXISTS` unless this is the session `place` and `options`
    /// ask for.
    fn refuse_unless_asked_for(
        &self,
        id: &str,
        place: &NewPlace,
        options: Option<&SessionOptions>,
    ) -> Result<(), PoolError> {
        let same_place = self.kind == history::entry::SessionKind::Primary
            && self.scene == place.scene.id
            && same_project(self.project.as_deref(), place.project.as_deref());
        let options_differ = if same_place {
            differing_options(options, self.created_with.as_ref(), !self.held_here)
        } else {
            Vec::new()
        };
        if same_place && options_differ.is_empty() {
            return Ok(());
        }
        Err(PoolError::SessionExists {
            session_id: id.to_string(),
            existing: Some(api::ExistingSession {
                scene: Some(self.scene.clone()),
                project_root: self.project.as_ref().map(|p| p.display().to_string()),
                session_kind: self.kind.into(),
                options_differ,
            }),
        })
    }
}

/// The fields of `asked` that are not what the session was `created` with,
/// an absent `options` being the same as an empty one. A recorder that
/// records and was given no `dir` was given the one it was created with. A
/// replay or rerun that the log alone cannot confirm (it is never recorded)
/// is not a difference.
fn differing_options(
    asked: Option<&SessionOptions>,
    created: Option<&SessionOptions>,
    replay_unrecorded: bool,
) -> Vec<String> {
    let asked = asked.cloned().unwrap_or_default();
    let created = created.cloned().unwrap_or_default();
    let mut differ = Vec::new();
    if !same_recorder(
        asked.recorder.as_ref(),
        created.recorder.as_ref(),
        replay_unrecorded,
    ) {
        differ.push("recorder".to_string());
    }
    let fields = |options: SessionOptions| match serde_json::to_value(SessionOptions {
        recorder: None,
        ..options
    }) {
        Ok(serde_json::Value::Object(fields)) => fields,
        _ => serde_json::Map::new(),
    };
    let (asked, created) = (fields(asked), fields(created));
    for name in SessionOptions::FIELDS {
        if asked.get(*name) != created.get(*name) {
            differ.push((*name).to_string());
        }
    }
    differ.sort();
    differ.dedup();
    differ
}

fn same_recorder(
    asked: Option<&RecorderOptions>,
    created: Option<&RecorderOptions>,
    replay_unrecorded: bool,
) -> bool {
    match (asked, created) {
        (None, None) => true,
        (Some(asked), None) => replay_unrecorded && !asked.records(),
        (None, Some(_)) => false,
        (Some(asked), Some(created)) => {
            asked.mode == created.mode
                && asked.strict == created.strict
                && asked.name == created.name
                && (asked.dir == created.dir || (asked.dir.is_none() && created.records()))
        }
    }
}

/// The same project, by path or by where the paths lead.
fn same_project(a: Option<&Path>, b: Option<&Path>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => {
            a == b
                || matches!(
                    (std::fs::canonicalize(a), std::fs::canonicalize(b)),
                    (Ok(a), Ok(b)) if a == b
                )
        }
        _ => false,
    }
}
