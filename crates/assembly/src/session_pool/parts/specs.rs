//! 每个会话的规格 `SessionSpec` 归 `SpecStore`：重建一个会话时照什么重来。内存里
//! 的那份是准的，日志里的 `agix.session/spec` 条目让它在会话出内存、守护进程重启
//! 之后还能找回来。
//!
//! 锁序：`specs` 和 `kept` 是两把 std 叶子锁，互不嵌套，持锁时不 `.await`。

use crate::session_pool::api::SessionOptions;
use serde_json::{Map, Value};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};
use tracing::warn;

const SPEC_NS: &str = "agix.session";
const SPEC_EVENT: &str = "spec";

/// What a rebuild of a session has to reproduce: the options it was created
/// with and what has been changed on it since.
///
/// An `Agent` is rebuilt whenever its binding goes stale, the janitor evicts
/// it, or the daemon restarts; every rebuild reproduces this, not whatever the
/// triggering call happens to pass.
#[derive(Clone, Default)]
pub(in crate::session_pool) struct SessionSpec {
    pub(in crate::session_pool) options: Option<SessionOptions>,
    pub(in crate::session_pool) model: Option<String>,
    pub(in crate::session_pool) provider: Option<String>,
    /// The session's layer as `session.setModel` and `session.setPrompt` left
    /// it; over `options.settings` once set. Kept apart from `options` so a
    /// `session.create` under the session's id still compares against what
    /// it was created with.
    pub(in crate::session_pool) settings: Option<Map<String, Value>>,
    /// Set by `session.setSkillDirs`; over `options.skill_dirs` once set.
    pub(in crate::session_pool) skill_dirs: Option<Vec<crate::session_pool::api::SkillDirOption>>,
    /// Set by `session.setPermissionMode` to a mode other than plan; over
    /// `options.settings.permissions.mode` once set. Clamped as the session is built.
    pub(in crate::session_pool) permission_mode: Option<base::permission::PermissionMode>,
    /// Shared by every Agent this process builds for the session, so a
    /// rebuild appends to the recording it was writing and continues a replay
    /// where it stood instead of from the first recorded call. Not persisted:
    /// a daemon that starts again starts the recording over.
    pub(in crate::session_pool) recorder: Option<Arc<telemetry::recorder::Recorder>>,
}

impl SessionSpec {
    /// The provider the conversation goes to: chosen since, else asked for
    /// at creation. `None` follows the settings' routing.
    pub(in crate::session_pool) fn provider(&self) -> Option<String> {
        self.provider
            .clone()
            .or_else(|| self.options.as_ref().and_then(|o| o.provider.clone()))
    }

    /// The session's own settings: changed since, else given at creation.
    pub(in crate::session_pool) fn settings_layer(&self) -> Option<&Map<String, Value>> {
        self.settings
            .as_ref()
            .or_else(|| self.options.as_ref().and_then(|o| o.settings.as_ref()))
    }

    /// Change the session's own settings, starting from those in force.
    pub(in crate::session_pool) fn change_layer(
        &mut self,
        change: impl FnOnce(&mut Map<String, Value>),
    ) {
        let mut layer = self.settings_layer().cloned().unwrap_or_default();
        change(&mut layer);
        self.settings = Some(layer);
    }

    /// One key of the session's own settings, dotted.
    pub(in crate::session_pool) fn session_setting(&self, dotted: &str) -> Option<&Value> {
        let mut segments = dotted.split('.');
        let first = self.settings_layer()?.get(segments.next()?)?;
        segments.try_fold(first, |node, segment| node.get(segment))
    }

    /// Whether the session is named after its first turn by its scene's
    /// naming prompt, and the prompt if so.
    pub(in crate::session_pool) fn naming_prompt(
        &self,
        scene: &base::interface::scene::SceneSpec,
        first_message: &str,
    ) -> Option<String> {
        if !self
            .options
            .as_ref()
            .is_none_or(SessionOptions::names_itself)
        {
            return None;
        }
        scene.session_name_prompt(first_message)
    }

    /// The host's skill directories: set since, else given at creation.
    pub(in crate::session_pool) fn skill_dirs(
        &self,
    ) -> Vec<crate::session_pool::api::SkillDirOption> {
        self.skill_dirs
            .clone()
            .or_else(|| self.options.as_ref().map(|o| o.skill_dirs.clone()))
            .unwrap_or_default()
    }

    /// The mode the session asks to run in outside plan: chosen since, else
    /// given at creation. `None` takes the settings'.
    pub(in crate::session_pool) fn requested_permission_mode(
        &self,
    ) -> Option<base::permission::PermissionMode> {
        self.permission_mode.or_else(|| {
            self.settings_layer()
                .and_then(crate::session_pool::api::requested_permission_mode)
        })
    }

    /// The conversation's model, by the same precedence as `provider`.
    pub(in crate::session_pool) fn model(&self) -> Option<String> {
        self.model
            .clone()
            .or_else(|| self.options.as_ref().and_then(|o| o.model.clone()))
    }
}

/// The persisted part of a [`SessionSpec`], written to the session's log as
/// an extension entry each time it changes; the last one wins. An extension
/// entry rather than a `Meta` field because the spec changes after `Meta` is
/// written, and readers that do not know the namespace skip it.
#[derive(Default, serde::Serialize, serde::Deserialize)]
pub(in crate::session_pool) struct RecordedSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(in crate::session_pool) options: Option<SessionOptions>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(in crate::session_pool) model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(in crate::session_pool) provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(in crate::session_pool) settings: Option<Map<String, Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(in crate::session_pool) skill_dirs: Option<Vec<crate::session_pool::api::SkillDirOption>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(in crate::session_pool) permission_mode: Option<base::permission::PermissionMode>,
}

pub(in crate::session_pool) struct SpecStore {
    /// Each session's spec, by id. Filled from the session's log the first
    /// time a session not built in this process is brought back.
    specs: Mutex<HashMap<String, SessionSpec>>,
    /// Sessions out of memory whose spec is still in `specs`, oldest first:
    /// those the log cannot give back (a recorder's progress, a session with
    /// no log). At most `cap` of them.
    kept: Mutex<VecDeque<String>>,
    history_store: Option<Arc<dyn history::store::HistoryStore>>,
    cap: usize,
}

impl SpecStore {
    pub(in crate::session_pool) fn new(
        history_store: Option<Arc<dyn history::store::HistoryStore>>,
        cap: usize,
    ) -> Self {
        Self {
            specs: Mutex::new(HashMap::new()),
            kept: Mutex::new(VecDeque::new()),
            history_store,
            cap,
        }
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<String, SessionSpec>> {
        self.specs
            .lock()
            .expect("specs mutex is never held across a panic")
    }

    fn lock_kept(&self) -> MutexGuard<'_, VecDeque<String>> {
        self.kept
            .lock()
            .expect("kept_specs is never held across a panic")
    }

    pub(in crate::session_pool) fn get(&self, session_id: &str) -> Option<SessionSpec> {
        self.lock().get(session_id).cloned()
    }

    /// The spec of a session being brought up: the one this process already
    /// holds, else the one `log` recorded (for a resume), else a new one
    /// from `options`. The session is in memory again, so its spec is no
    /// longer one of the kept.
    pub(in crate::session_pool) async fn spec_for(
        &self,
        session_id: &str,
        log: Option<&[history::entry::EnvelopedEntry]>,
        options: Option<&SessionOptions>,
    ) -> SessionSpec {
        let spec = self.resolve(session_id, log, options).await;
        self.lock_kept().retain(|k| k != session_id);
        spec
    }

    async fn resolve(
        &self,
        session_id: &str,
        log: Option<&[history::entry::EnvelopedEntry]>,
        options: Option<&SessionOptions>,
    ) -> SessionSpec {
        if let Some(spec) = self.get(session_id) {
            return spec;
        }
        let resume = log.is_some();
        let mut spec = log.map(recorded_in).unwrap_or_default();
        // A log written before specs were recorded has none; the options of
        // the call that brought the session back are the best there is, and
        // are recorded so the next resume finds them too.
        let mut adopted = resume && spec.options.is_none() && options.is_some();
        if spec.options.is_none() {
            spec.options = options.cloned();
        }
        // Only recording is written to the log; a replay or rerun has to be
        // asked for again by the call that brings the session back.
        let logged_recording = spec
            .options
            .as_ref()
            .and_then(|o| o.recorder.as_ref())
            .is_some_and(|r| r.records());
        if !logged_recording {
            if let Some(recorder) = options.and_then(|o| o.recorder.clone()) {
                adopted |= resume && recorder.records();
                spec.options.get_or_insert_with(Default::default).recorder = Some(recorder);
            }
        }
        if spec.options.as_ref().is_some_and(|o| o.recorder.is_some()) {
            spec.recorder = Some(telemetry::recorder::Recorder::new());
        }
        let spec = self
            .lock()
            .entry(session_id.to_string())
            .or_insert(spec)
            .clone();
        if adopted {
            self.persist(session_id, &spec).await;
        }
        spec
    }

    /// Have the spec of `session_id` in memory, from its log if this process
    /// holds none.
    pub(in crate::session_pool) async fn load(&self, session_id: &str) {
        if self.lock().contains_key(session_id) {
            return;
        }
        let recorded = self.recorded(session_id).await;
        self.lock()
            .entry(session_id.to_string())
            .or_insert(recorded);
    }

    /// The spec the log of `session_id` recorded last; an empty one when it
    /// recorded none or cannot be read.
    pub(in crate::session_pool) async fn recorded(&self, session_id: &str) -> SessionSpec {
        let entries = match (
            self.history_store.as_ref(),
            base::session::SessionId::parse(session_id),
        ) {
            (Some(store), Ok(sid)) => store.load(sid).await.unwrap_or_default(),
            _ => Vec::new(),
        };
        recorded_in(&entries)
    }

    pub(in crate::session_pool) async fn persist(&self, session_id: &str, spec: &SessionSpec) {
        if let Err(e) = self.try_persist(session_id, spec).await {
            warn!(%session_id, error = %e, "failed to record the session spec");
        }
    }

    /// [`Self::persist`], saying whether the log took it.
    pub(in crate::session_pool) async fn try_persist(
        &self,
        session_id: &str,
        spec: &SessionSpec,
    ) -> Result<(), String> {
        let Some(store) = self.history_store.as_ref() else {
            return Ok(());
        };
        let Ok(sid) = base::session::SessionId::parse(session_id) else {
            return Ok(());
        };
        let recorded = RecordedSpec {
            options: spec.options.clone().map(|mut o| {
                o.recorder = o.recorder.filter(|r| r.records());
                o
            }),
            model: spec.model.clone(),
            provider: spec.provider.clone(),
            settings: spec.settings.clone(),
            skill_dirs: spec.skill_dirs.clone(),
            permission_mode: spec.permission_mode,
        };
        let payload = serde_json::to_value(&recorded).map_err(|e| e.to_string())?;
        let entry = history::entry::LogEntry::Extension {
            ns: SPEC_NS.to_string(),
            event: SPEC_EVENT.to_string(),
            payload,
        };
        store.append(sid, entry).await.map_err(|e| e.to_string())
    }

    /// Change this session's spec in memory only, returning it as it was and
    /// as it is now. One this process does not hold is read from the log
    /// first, so the change is made to what the session was given and never
    /// to an empty spec standing in for it.
    pub(in crate::session_pool) async fn update(
        &self,
        session_id: &str,
        change: impl FnOnce(&mut SessionSpec),
    ) -> (Option<SessionSpec>, SessionSpec) {
        loop {
            if let Some(spec) = self.lock().get_mut(session_id) {
                let previous = spec.clone();
                change(spec);
                return (Some(previous), spec.clone());
            }
            self.load(session_id).await;
        }
    }

    /// Put back what [`Self::update`] returned as the spec before.
    pub(in crate::session_pool) fn restore(&self, session_id: &str, previous: Option<SessionSpec>) {
        let mut specs = self.lock();
        match previous {
            Some(previous) => specs.insert(session_id.to_string(), previous),
            None => specs.remove(session_id),
        };
    }

    /// Drop the spec of `sid`, now out of memory, when its log gives it
    /// back; one the log cannot — a recorder's progress, a session with no
    /// log — is kept, for at most `cap` sessions.
    pub(in crate::session_pool) fn release(&self, sid: &str) {
        let logged = self.history_store.is_some() && base::session::SessionId::parse(sid).is_ok();
        let records = self
            .lock()
            .get(sid)
            .is_some_and(|spec| spec.recorder.is_some());
        if logged && !records {
            self.forget(sid);
            return;
        }
        let oldest = {
            let mut kept = self.lock_kept();
            kept.retain(|k| k != sid);
            kept.push_back(sid.to_string());
            if kept.len() > self.cap {
                kept.pop_front()
            } else {
                None
            }
        };
        if let Some(oldest) = oldest {
            self.forget(&oldest);
        }
    }

    /// Forget a session's spec — it is deleted or closed. A deleted id
    /// created again starts from the options it is created with.
    pub(in crate::session_pool) fn forget(&self, session_id: &str) {
        self.lock().remove(session_id);
    }

    #[cfg(test)]
    pub(in crate::session_pool) fn contains(&self, session_id: &str) -> bool {
        self.lock().contains_key(session_id)
    }

    #[cfg(test)]
    pub(in crate::session_pool) fn with_mut<T>(
        &self,
        session_id: &str,
        f: impl FnOnce(&mut SessionSpec) -> T,
    ) -> T {
        f(self.lock().entry(session_id.to_string()).or_default())
    }

    #[cfg(test)]
    pub(in crate::session_pool) fn kept_len(&self) -> usize {
        self.lock_kept().len()
    }
}

/// Whether `entry` is a recorded spec.
pub(in crate::session_pool) fn is_spec(entry: &history::entry::LogEntry) -> bool {
    matches!(entry, history::entry::LogEntry::Extension { ns, event, .. }
        if ns == SPEC_NS && event == SPEC_EVENT)
}

fn last_recorded(entries: &[history::entry::EnvelopedEntry]) -> Option<RecordedSpec> {
    entries.iter().rev().find_map(|env| match &env.entry {
        history::entry::LogEntry::Extension { ns, event, payload }
            if ns == SPEC_NS && event == SPEC_EVENT =>
        {
            let mut payload = payload.clone();
            if let Value::Object(spec) = &mut payload {
                spec_of_0_3_and_0_4_0(spec);
            }
            serde_json::from_value::<RecordedSpec>(payload).ok()
        }
        _ => None,
    })
}

/// A spec 0.3 or 0.4.0 recorded, as it is written now. In `options`, the
/// `effort`, `thinking` (0.3: `thinking_mode`) and `prompt` are
/// `settings.model.effort`, `settings.model.thinking` and
/// `settings.prompt.{append, override}`, and 0.3's `turn`, `permission_mode`
/// and `permission_rules` are `settings.turn`, `settings.permissions.mode` and
/// the rule lists of `settings.permissions`. The `effort`, `thinking_mode` and
/// `prompt` set on the session since are those keys of the spec's own
/// `settings`, a `null` among them a key taken out. A session keeps what it
/// was created and changed with; options sent in an old shape are refused.
fn spec_of_0_3_and_0_4_0(spec: &mut Map<String, Value>) {
    if let Some(Value::Object(options)) = spec.get_mut("options") {
        options_of_0_4_0(options);
        options_of_0_3(options);
    }
    let effort = spec.remove("effort");
    let thinking = spec.remove("thinking_mode");
    let prompt = spec.remove("prompt");
    if effort.is_none() && thinking.is_none() && prompt.is_none() {
        if spec.get("settings").is_some_and(|layer| !layer.is_object()) {
            spec.remove("settings");
        }
        return;
    }
    let created = spec
        .get("options")
        .and_then(|options| options.get("settings"))
        .cloned();
    let mut layer = match spec.remove("settings").or(created) {
        Some(Value::Object(layer)) => layer,
        _ => Map::new(),
    };
    for (key, value) in [("effort", effort), ("thinking", thinking)] {
        if let Some(value) = value {
            set_in_layer(
                &mut layer,
                &["model", key],
                Some(value).filter(|v| !v.is_null()),
            );
        }
    }
    if let Some(Value::Object(prompt)) = prompt {
        for key in ["append", "override"] {
            let value = prompt.get(key).cloned().filter(|v| !v.is_null());
            set_in_layer(&mut layer, &["prompt", key], value);
        }
    }
    spec.insert("settings".into(), Value::Object(layer));
}

fn options_of_0_4_0(options: &mut Map<String, Value>) {
    let mut model = Vec::new();
    for (old, key) in [
        ("effort", "effort"),
        ("thinking", "thinking"),
        ("thinking_mode", "thinking"),
    ] {
        if let Some(value) = options.remove(old) {
            model.push((key, value));
        }
    }
    let prompt = options.remove("prompt");
    if model.is_empty() && prompt.is_none() {
        return;
    }
    let Some(layer) = object_at(options, "settings") else {
        return;
    };
    for (key, value) in model {
        set_in_layer(layer, &["model", key], Some(value).filter(|v| !v.is_null()));
    }
    if let Some(Value::Object(prompt)) = prompt {
        for key in ["append", "override"] {
            if let Some(value) = prompt.get(key).filter(|v| !v.is_null()) {
                set_in_layer(layer, &["prompt", key], Some(value.clone()));
            }
        }
    }
    if layer.is_empty() {
        options.remove("settings");
    }
}

/// The object under `key`, made when there is none; `None` when something
/// else is there.
fn object_at<'a>(map: &'a mut Map<String, Value>, key: &str) -> Option<&'a mut Map<String, Value>> {
    map.entry(key)
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
}

/// Set the setting at `path` in `layer`, or take it out with `None`; an
/// object left empty by taking one out goes with it. A segment that holds
/// something other than an object is left as it is.
pub(in crate::session_pool) fn set_in_layer(
    layer: &mut Map<String, Value>,
    path: &[&str],
    value: Option<Value>,
) {
    let Some((first, rest)) = path.split_first() else {
        return;
    };
    if rest.is_empty() {
        match value {
            Some(value) => layer.insert((*first).to_string(), value),
            None => layer.remove(*first),
        };
        return;
    }
    if value.is_none() && !layer.contains_key(*first) {
        return;
    }
    let Some(inner) = object_at(layer, first) else {
        return;
    };
    set_in_layer(inner, rest, value);
    if inner.is_empty() {
        layer.remove(*first);
    }
}

fn options_of_0_3(options: &mut Map<String, Value>) {
    let (turn, mode, rules) = (
        options.remove("turn"),
        options.remove("permission_mode"),
        options.remove("permission_rules"),
    );
    if turn.is_none() && mode.is_none() && rules.is_none() {
        return;
    }
    let Some(layer) = object_at(options, "settings") else {
        return;
    };
    if let Some(turn) = turn {
        layer.insert("turn".into(), turn);
    }
    if mode.is_none() && rules.is_none() {
        return;
    }
    let Some(permissions) = object_at(layer, "permissions") else {
        return;
    };
    if let Some(mode) = mode {
        permissions.insert("mode".into(), mode);
    }
    for rule in rules.iter().filter_map(Value::as_array).flatten() {
        let (Some(list), Some(tool)) = (
            rule.get("behavior").and_then(Value::as_str),
            rule.get("tool_name").and_then(Value::as_str),
        ) else {
            continue;
        };
        let text = match rule.get("rule_content").and_then(Value::as_str) {
            Some(content) => format!("{tool}({content})"),
            None => tool.to_string(),
        };
        match permissions.get_mut(list).and_then(Value::as_array_mut) {
            Some(entries) => entries.push(Value::String(text)),
            None => {
                permissions.insert(list.into(), Value::Array(vec![Value::String(text)]));
            }
        }
    }
}

/// The spec entry a fork appends after copying `copied`: the last spec it
/// copied without its recording's name, so the fork records under its own id
/// instead of starting its source's recording over. `None` when that spec
/// names no recording.
pub(in crate::session_pool) fn forked_spec(
    copied: &[history::entry::EnvelopedEntry],
) -> Option<history::entry::LogEntry> {
    let mut spec = last_recorded(copied)?;
    spec.options.as_mut()?.recorder.as_mut()?.name.take()?;
    Some(history::entry::LogEntry::Extension {
        ns: SPEC_NS.to_string(),
        event: SPEC_EVENT.to_string(),
        payload: serde_json::to_value(&spec).ok()?,
    })
}

/// The spec `entries`, a session's log, recorded last; an empty one when they
/// record none. When the last one does not read, the one before it is taken,
/// with the last one's `plugins` settings: those only narrow, so a session
/// whose spec cannot say what it narrowed to gets no plugin rather than all.
pub(in crate::session_pool) fn recorded_in(
    entries: &[history::entry::EnvelopedEntry],
) -> SessionSpec {
    let recorded = last_recorded(entries).unwrap_or_default();
    let mut spec = SessionSpec {
        options: recorded.options,
        model: recorded.model,
        provider: recorded.provider,
        settings: recorded.settings,
        skill_dirs: recorded.skill_dirs,
        permission_mode: recorded.permission_mode,
        recorder: None,
    };
    if let Some(plugins) = plugins_of_unread_last(entries) {
        spec.change_layer(|layer| {
            layer.insert("plugins".into(), plugins);
        });
    }
    spec
}

/// The `plugins` settings of the last spec in `entries`, when it does not
/// read as a spec and sets them; `{"enabled": false}` when where they would be
/// does not read either.
fn plugins_of_unread_last(entries: &[history::entry::EnvelopedEntry]) -> Option<Value> {
    let closed = || {
        Some(Value::Object(Map::from_iter([(
            "enabled".to_string(),
            Value::Bool(false),
        )])))
    };
    let mut payload = entries.iter().rev().find_map(|env| match &env.entry {
        history::entry::LogEntry::Extension { ns, event, payload }
            if ns == SPEC_NS && event == SPEC_EVENT =>
        {
            Some(payload.clone())
        }
        _ => None,
    })?;
    let Value::Object(spec) = &mut payload else {
        return closed();
    };
    spec_of_0_3_and_0_4_0(spec);
    if serde_json::from_value::<RecordedSpec>(Value::Object(spec.clone())).is_ok() {
        return None;
    }
    let layer = match (spec.get("settings"), spec.get("options")) {
        (Some(Value::Object(layer)), _) => layer,
        (Some(_), _) => return closed(),
        (None, None) => return None,
        (None, Some(Value::Object(options))) => match options.get("settings") {
            None => return None,
            Some(Value::Object(layer)) => layer,
            Some(_) => return closed(),
        },
        (None, Some(_)) => return closed(),
    };
    layer.get("plugins").cloned()
}
