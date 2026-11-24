//! 对已有会话的元数据操作：重命名、追加与列出扩展条目、换模型、换提示词。换模型的
//! effort / thinking 与换提示词都写进会话自己的那层设置，和 `options.settings` 一样判。

use crate::session_pool::parts::bindings::ProjectBinding;
use crate::session_pool::parts::factory::held_keys;
use crate::session_pool::parts::specs::set_in_layer;
use crate::session_pool::*;

impl SessionPool {
    /// `session.rename` — what the person looking at this session calls it.
    ///
    /// `None` clears the name, putting the session back under whatever names
    /// it automatically. The name is written where the session's own facts
    /// live, so every host sharing `~/.agix` sees it — a name each host kept
    /// in an overlay of its own is a name that differs depending on which
    /// window you have open.
    ///
    /// Works on a session that is not in memory: naming is a fact about the
    /// session, not an operation on a running engine, so it does not resume
    /// anything to do it.
    pub async fn rename_session(
        &self,
        session_id: &str,
        name: Option<&str>,
    ) -> Result<api::Renamed, PoolError> {
        self.refuse_scene_mismatch(session_id).await?;
        let name = match name {
            None => None,
            Some(raw) => {
                let trimmed = raw.trim();
                if trimmed.is_empty() {
                    return Err(PoolError::InvalidRequest(
                        "name must not be empty; pass null to clear it".into(),
                    ));
                }
                if trimmed.chars().count() > Self::MAX_SESSION_NAME {
                    return Err(PoolError::InvalidRequest(format!(
                        "name must be at most {} characters",
                        Self::MAX_SESSION_NAME
                    )));
                }
                // A host renders this in a list. A newline or a control
                // character in it is not a name, it is a rendering bug
                // somebody else has to find.
                if trimmed.chars().any(char::is_control) {
                    return Err(PoolError::InvalidRequest(
                        "name must not contain control characters".into(),
                    ));
                }
                Some(trimmed.to_string())
            }
        };

        let store = self.directory.require_store()?;
        let sid = base::session::SessionId::parse(session_id)
            .map_err(|_| PoolError::InvalidRequest(format!("invalid session_id: {session_id}")))?;
        // Naming something that does not exist would create a sidecar for a
        // session nothing else knows about — a row in every future listing
        // that nobody can open.
        let live = self.live.core_of(session_id).await;
        if live.is_none()
            && matches!(
                store.load(sid).await,
                Err(history::error::HistoryError::SessionNotFound(_))
            )
        {
            return Err(PoolError::session_not_found(session_id));
        }

        store
            .set_session_title(sid, name.clone())
            .await
            .map_err(|e| PoolError::Internal(format!("could not rename: {e}")))?;

        if let Some(core) = self.live.core_of(session_id).await {
            let mut naming = core.naming();
            naming.name = name.clone();
            naming.named_by_user = name.is_some();
        }

        Ok(api::Renamed {
            session_id: session_id.to_string(),
            name,
        })
    }

    const MAX_ENTRY_NS: usize = 128;

    pub async fn append_entry(
        &self,
        session_id: &str,
        ns: &str,
        event: &str,
        payload: serde_json::Value,
    ) -> Result<api::EntryView, PoolError> {
        self.refuse_scene_mismatch(session_id).await?;
        if ns.is_empty() || ns.len() > Self::MAX_ENTRY_NS || ns.chars().any(char::is_control) {
            return Err(PoolError::InvalidRequest(format!(
                "ns must be 1–{} characters with no control characters",
                Self::MAX_ENTRY_NS
            )));
        }
        if ns.starts_with(ENGINE_NS_PREFIX) {
            return Err(PoolError::InvalidRequest(format!(
                "the `{ENGINE_NS_PREFIX}*` namespaces are the engine's own"
            )));
        }
        if event.is_empty() || event.chars().any(char::is_control) {
            return Err(PoolError::InvalidRequest(
                "event must be a non-empty name".into(),
            ));
        }
        let store = self.directory.require_store()?;
        let sid = base::session::SessionId::parse(session_id)
            .map_err(|_| PoolError::InvalidRequest(format!("invalid session_id: {session_id}")))?;
        // Appending to a session that does not exist would create a log for it.
        self.directory.load_entries(session_id).await?;
        let entry = history::entry::LogEntry::Extension {
            ns: ns.to_string(),
            event: event.to_string(),
            payload: payload.clone(),
        };
        store
            .append(sid, entry)
            .await
            .map_err(|e| PoolError::Internal(format!("could not append: {e}")))?;
        if let Err(e) = store.sync(sid).await {
            warn!(%session_id, error = %e, "an appended entry was not synced");
        }

        let entries = self.directory.load_entries(session_id).await?;
        let lengths = history::transcript::projected_lengths_with(&entries, store.projection());
        let written = entries
            .iter()
            .enumerate()
            .rev()
            .find_map(|(i, env)| match &env.entry {
                history::entry::LogEntry::Extension {
                    ns: n,
                    event: e,
                    payload: p,
                } if n == ns && e == event && *p == payload => Some(entry_view(i, env, &lengths)),
                _ => None,
            })
            .flatten()
            .ok_or_else(|| {
                PoolError::Internal("the entry was not found after writing it".to_string())
            })?;

        self.publish_between_turns(
            session_id,
            SessionEvent::ExtensionEntry(serde_json::to_value(&written).unwrap_or_default()),
        )
        .await;
        Ok(written)
    }

    pub async fn list_entries(
        &self,
        session_id: &str,
        namespaces: Option<&[String]>,
    ) -> Result<api::EntryList, PoolError> {
        let store = self.directory.require_store()?;
        let entries = self.directory.load_entries(session_id).await?;
        let lengths = history::transcript::projected_lengths_with(&entries, store.projection());
        let listed: Vec<api::EntryView> = entries
            .iter()
            .enumerate()
            .filter(|(_, env)| match &env.entry {
                history::entry::LogEntry::Extension { ns, .. } => {
                    !ns.starts_with(ENGINE_NS_PREFIX)
                        && namespaces.is_none_or(|wanted| wanted.iter().any(|w| w == ns))
                }
                _ => false,
            })
            .filter_map(|(i, env)| entry_view(i, env, &lengths))
            .collect();
        Ok(api::EntryList { entries: listed })
    }

    /// `session.setModel` — what the next API call this session makes should
    /// ask for.
    ///
    /// The choice is written into the session's spec and onto its state,
    /// which every engine the session runs on reads as a turn starts — so it
    /// holds whichever engine runs the next turn, a rebuild in between
    /// included. The transcript is not touched: this is a change to what
    /// answers the next message, not a rewrite of the conversation that led to
    /// it.
    ///
    /// Refused while a turn is running, rather than queued behind it. The
    /// engine's main loop is inside `process_turn` for the duration and is
    /// not reading its input channel, so a command accepted here would take
    /// effect at some unannounced later moment — the caller would have been
    /// told the model changed and watched the old one answer.
    ///
    /// `model` is a tier — `strong`, `balanced`, `fast` — or a model id. With
    /// `provider`, that provider goes first in the session's `model.order`;
    /// an engine is built around its router, so the session is rebuilt on the
    /// new spec; it is idle, so nothing is cut off. A model id no provider in
    /// the session's order takes is refused — sending a name to an endpoint
    /// that does not serve it fails only later, and less legibly.
    ///
    /// `reasoning` changes `model.effort` and `model.thinking` in the
    /// session's own settings — for its sub-agents and team members too —
    /// judged as `options.settings` is; the session is rebuilt on them, as
    /// for a new provider.
    pub async fn set_session_model(
        &self,
        session_id: &str,
        model: Option<&str>,
        provider: Option<&str>,
        reasoning: api::ReasoningChange,
    ) -> Result<api::ModelChoice, PoolError> {
        self.refuse_scene_mismatch(session_id).await?;
        self.wait_out_finishing_turn(session_id).await;
        let (project, scene) = self
            .live
            .with_live(session_id, |live| {
                if let Some(busy) = live.turn.busy(session_id) {
                    return Err(PoolError::SessionBusy(busy));
                }
                live.core.hub().touch();
                Ok((
                    live.core.project().map(Path::to_path_buf),
                    live.core.scene().id.clone(),
                ))
            })
            .await
            .ok_or_else(|| PoolError::session_not_found(session_id))??;

        // Under the session lock, as every change to a session's spec is: a
        // rebuild's change and this one land, and are undone, one at a time.
        let reasoning_changes = !reasoning.is_empty();
        let (spec, clamped_options) = self
            .live
            .with_session_lock(session_id, async {
                // A close or delete queued ahead of this on the lock has
                // taken the session since the check above.
                if !self.live.contains(session_id).await {
                    return Err(PoolError::session_not_found(session_id));
                }
                let current_provider = self.specs.get(session_id).and_then(|spec| spec.provider());
                let binding = self.binding_for(project.as_deref(), &scene).await;
                binding
                    .check_model_choice(provider.or(current_provider.as_deref()), model)
                    .map_err(PoolError::InvalidRequest)?;

                let switching_provider =
                    provider.is_some_and(|p| current_provider.as_deref() != Some(p));
                let change = |spec: &mut SessionSpec| {
                    choose_model(spec, model, provider, switching_provider);
                    choose_reasoning(spec, reasoning);
                };

                let rebuild = switching_provider || reasoning_changes;
                let spec = self.change_model_spec(session_id, rebuild, change).await?;
                let held = held_keys(&binding.settings, &spec, &[EFFORT, THINKING]);
                Ok((spec, held))
            })
            .await?;
        Ok(api::ModelChoice {
            session_id: session_id.to_string(),
            model: spec.model(),
            provider: spec.provider(),
            effort: own_setting(&spec, EFFORT),
            thinking: own_setting(&spec, THINKING),
            clamped_options,
            applies: api::Applies::NextTurn,
        })
    }

    /// `change` made to `sid`'s spec and recorded; with `rebuild`, the session
    /// rebuilt on it. A provider is a different client and the session's
    /// settings are read as its engine is built, so either is a rebuild
    /// rather than something an engine is handed mid-life. The turn slot is
    /// taken before the change is made, so a turn that started since the
    /// caller's idle check refuses both rather than being cut off or finding
    /// the change applied by some later rebuild.
    async fn change_model_spec(
        &self,
        sid: &str,
        rebuild: bool,
        change: impl FnOnce(&mut SessionSpec),
    ) -> Result<SessionSpec, PoolError> {
        if !rebuild {
            let spec = self.update_spec(sid, change).await;
            self.specs.persist(sid, &spec).await;
            return Ok(spec);
        }
        let held = self
            .hold_for_rebuild(sid)
            .await
            .map_err(|e| e.into_error(sid))?;
        self.rebuild_on_changed_spec(sid, held, change)
            .await
            .map_err(|e| e.into_error(sid))
    }

    /// Change `sid`'s spec, and put the model it now names onto the session's
    /// state when the session is in memory.
    pub(in crate::session_pool) async fn update_spec(
        &self,
        sid: &str,
        change: impl FnOnce(&mut SessionSpec),
    ) -> SessionSpec {
        let spec = self.specs.update(sid, change).await.1;
        self.project_model_choice(sid).await;
        spec
    }

    /// Put `sid`'s model choice onto its session's state, a tier as the
    /// model the session's first provider serves it with.
    pub(in crate::session_pool) async fn project_model_choice(&self, sid: &str) {
        let spec = self.specs.get(sid);
        if let Some(core) = self.live.core_of(sid).await {
            let model = match spec.as_ref().and_then(SessionSpec::model) {
                Some(name) if base::provider::Tier::from_name(&name).is_some() => {
                    let binding = self.binding_for(core.project(), &core.scene().id).await;
                    let provider = spec.as_ref().and_then(SessionSpec::provider);
                    let resolved = binding
                        .task_router
                        .session(provider.as_deref())
                        .map(|router| base::provider::TaskRouting::resolve_model(&router, &name));
                    Some(resolved.unwrap_or(name))
                }
                other => other,
            };
            core.state().set_model_choice(model);
        }
    }

    /// `session.setPrompt` — `prompt.append` and `prompt.override` in this
    /// session's own settings from its next turn on, judged as
    /// `options.settings` is. Refused while a turn runs, like `setModel`; an
    /// idle session is rebuilt on the new prompt at once, its history kept.
    pub async fn set_session_prompt(
        &self,
        session_id: &str,
        change: api::PromptChange,
    ) -> Result<api::PromptChoice, PoolError> {
        self.refuse_scene_mismatch(session_id).await?;
        self.wait_out_finishing_turn(session_id).await;
        if let Some(busy) = self
            .live
            .with_live(session_id, |l| l.turn.busy(session_id))
            .await
            .flatten()
        {
            return Err(PoolError::SessionBusy(busy));
        }
        if !self.session_exists(session_id).await {
            return Err(PoolError::session_not_found(session_id));
        }
        let append = change
            .append
            .map(|append| append.and_then(|a| serde_json::to_value(a).ok()));
        let override_text = change
            .override_text
            .map(|text| text.map(serde_json::Value::String));
        let change = |spec: &mut SessionSpec| {
            spec.change_layer(|layer| {
                for (key, value) in [(APPEND, &append), (OVERRIDE, &override_text)] {
                    if let Some(value) = value {
                        set_in_layer(layer, &dotted(key), value.clone());
                    }
                }
            });
        };
        let spec = self
            .live
            .with_session_lock(session_id, async {
                let spec = match self.hold_for_rebuild(session_id).await {
                    Ok(held) => {
                        self.rebuild_on_changed_spec(session_id, held, change)
                            .await?
                    }
                    Err(Rebuild::NotLive) if !self.session_exists(session_id).await => {
                        self.specs.forget(session_id);
                        return Err(Rebuild::NotLive);
                    }
                    Err(Rebuild::NotLive) => {
                        let spec = self.specs.update(session_id, change).await.1;
                        self.specs.persist(session_id, &spec).await;
                        self.specs.release(session_id);
                        spec
                    }
                    Err(busy) => return Err(busy),
                };
                Ok(spec)
            })
            .await
            .map_err(|e| e.into_error(session_id))?;
        let clamped_options = match self.binding_of_session(session_id).await {
            Some(binding) => held_keys(&binding.settings, &spec, &[APPEND, OVERRIDE]),
            None => Vec::new(),
        };
        Ok(api::PromptChoice {
            session_id: session_id.to_string(),
            append: own_setting(&spec, APPEND),
            override_text: own_setting(&spec, OVERRIDE),
            clamped_options,
            applies: api::Applies::NextTurn,
        })
    }

    /// The binding `session_id` runs on, or would be brought back on.
    async fn binding_of_session(&self, session_id: &str) -> Option<Arc<ProjectBinding>> {
        if let Some(binding) = self.live_binding(session_id).await {
            return Some(binding);
        }
        let log = self.directory.load_entries(session_id).await.ok()?;
        Some(self.binding_of_log(&log).await.0)
    }
}

impl SessionPool {
    /// `session.setSkillDirs` — the host's skill directories for the session,
    /// all of them at once; `[]` clears them. Nothing is rebuilt: a session in
    /// memory takes them up in place, its running delegates included, and
    /// shows the change at the start of its next turn. A session out of
    /// memory only has its recorded spec changed, and takes them up when it
    /// is brought back. Checked as `options.skill_dirs` is at creation.
    pub async fn set_skill_dirs(
        &self,
        session_id: &str,
        dirs: Vec<api::SkillDirOption>,
    ) -> Result<Vec<api::SkillDirOption>, PoolError> {
        self.refuse_scene_mismatch(session_id).await?;
        if !self.session_exists(session_id).await {
            return Err(PoolError::session_not_found(session_id));
        }
        let change = |spec: &mut SessionSpec| spec.skill_dirs = Some(dirs.clone());
        self.live
            .with_session_lock(session_id, async {
                match self.live.core_of(session_id).await {
                    Some(core) => {
                        let binding = self.binding_for(core.project(), &core.scene().id).await;
                        let accepted = HostDirTable::check(&dirs, &binding.settings)
                            .map_err(PoolError::InvalidOptions)?;
                        let spec = self.specs.update(session_id, change).await.1;
                        self.specs.persist(session_id, &spec).await;
                        self.take_up_skill_dirs(&core, &spec, accepted, Vec::new(), &binding);
                    }
                    None => {
                        let log = self.directory.load_entries(session_id).await?;
                        let (binding, _, _) = self.binding_of_log(&log).await;
                        HostDirTable::check(&dirs, &binding.settings)
                            .map_err(PoolError::InvalidOptions)?;
                        let spec = self.specs.update(session_id, change).await.1;
                        self.specs.persist(session_id, &spec).await;
                        self.specs.release(session_id);
                    }
                }
                Ok(dirs.clone())
            })
            .await
    }
}

const EFFORT: &str = "model.effort";
const THINKING: &str = "model.thinking";
const APPEND: &str = "prompt.append";
const OVERRIDE: &str = "prompt.override";

fn dotted(key: &str) -> Vec<&str> {
    key.split('.').collect()
}

/// `key` of `spec`'s own settings, as `T`; `None` when it is not set there or
/// does not read.
fn own_setting<T: serde::de::DeserializeOwned>(spec: &SessionSpec, key: &str) -> Option<T> {
    serde_json::from_value(spec.session_setting(key)?.clone()).ok()
}

fn choose_reasoning(spec: &mut SessionSpec, change: api::ReasoningChange) {
    if change.is_empty() {
        return;
    }
    let effort = change
        .effort
        .map(|effort| effort.and_then(|e| serde_json::to_value(e).ok()));
    let thinking = change
        .thinking
        .map(|thinking| thinking.and_then(|t| serde_json::to_value(t).ok()));
    spec.change_layer(|layer| {
        for (key, value) in [(EFFORT, effort), (THINKING, thinking)] {
            if let Some(value) = value {
                set_in_layer(layer, &dotted(key), value);
            }
        }
    });
}

fn choose_model(
    spec: &mut SessionSpec,
    model: Option<&str>,
    provider: Option<&str>,
    switching_provider: bool,
) {
    if let Some(p) = provider {
        spec.provider = Some(p.to_string());
        // A model chosen for the old provider means nothing to the new one;
        // without a new choice the new provider's default applies.
        if switching_provider {
            spec.model = None;
        }
    }
    if let Some(m) = model {
        spec.model = Some(m.to_string());
    }
}
