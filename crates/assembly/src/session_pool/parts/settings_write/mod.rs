//! 改写 settings 目录里的文件：持目录锁、在阻塞线程上执行；以及按层写设置的那一份
//! 读改写——读出一层的两个文件、套用 `settings::edit` 的改动、按写成的样子合并整对
//! 设置、逐键判拒绝、通过才落盘。`config.set`、`config.setProvider`、`mcp.*` 都经它写。

mod keys;

pub(in crate::session_pool) use keys::{refusal, CONFIG_SET_MAX_KEYS};

use std::path::{Path, PathBuf};

use base::paths::entry;
use settings::edit::{EditOp, KeyPath, TierFiles};
use settings::LoadedSettings;

use crate::session_pool::api::{self, PoolError, RejectKind, SettingRejected};
use crate::session_pool::parts::bindings::PairReader;

pub(in crate::session_pool) enum SettingsEditError {
    Locked(String),
    Failed(String),
}

impl SettingsEditError {
    pub(in crate::session_pool) fn into_pool(self, failed: fn(String) -> PoolError) -> PoolError {
        match self {
            Self::Locked(message) => PoolError::SettingsLocked(message),
            Self::Failed(message) => failed(message),
        }
    }
}

impl From<SettingsEditError> for PoolError {
    fn from(e: SettingsEditError) -> Self {
        e.into_pool(PoolError::InvalidRequest)
    }
}

/// Run `edit` holding the settings directory `settings_file` is in — see
/// `settings::file` — on a blocking thread, since the lock can wait on
/// another process, for a bounded time.
pub(in crate::session_pool) async fn edit_settings_dir<R: Send + 'static>(
    settings_file: &Path,
    edit: impl FnOnce(&settings::file::SettingsDir) -> Result<R, String> + Send + 'static,
) -> Result<R, SettingsEditError> {
    let dir = settings_file
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default();
    tokio::task::spawn_blocking(move || {
        let tier = settings::file::SettingsDir::lock(&dir).map_err(|e| match e {
            locked @ settings::file::SettingsFileError::Locked { .. } => {
                SettingsEditError::Locked(locked.to_string())
            }
            other => SettingsEditError::Failed(other.to_string()),
        })?;
        edit(&tier).map_err(SettingsEditError::Failed)
    })
    .await
    .map_err(|e| SettingsEditError::Failed(format!("writing settings did not finish: {e}")))?
}

/// What a write does to MCP connections once it has landed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::session_pool) enum McpEffect {
    None,
    /// Every server of the pairs the tier is under connects anew.
    Full,
    /// Only this server is looked at again, wherever it is connected; with
    /// `connect_pair`, the pair written for connects whole first if it never
    /// did.
    // Only the MCP management methods make one, and a build without `mcp` has none.
    #[allow(dead_code)]
    Server {
        name: String,
        connect_pair: bool,
    },
}

/// A check of the tier's two files as read under the lock, before anything
/// is applied to them; told where its `settings.json` is.
pub(in crate::session_pool) type Precondition =
    Box<dyn FnOnce(&TierFiles, &Path) -> Result<(), String> + Send>;

/// One write to one settings tier of a pair.
pub(in crate::session_pool) struct TierEdit {
    pub(in crate::session_pool) tier: api::SettingsTier,
    pub(in crate::session_pool) ops: Vec<EditOp>,
    pub(in crate::session_pool) if_revision: Option<String>,
    pub(in crate::session_pool) dry_run: bool,
    pub(in crate::session_pool) precondition: Option<Precondition>,
    pub(in crate::session_pool) mcp: McpEffect,
}

impl TierEdit {
    pub(in crate::session_pool) fn new(tier: api::SettingsTier, ops: Vec<EditOp>) -> Self {
        Self {
            tier,
            ops,
            if_revision: None,
            dry_run: false,
            precondition: None,
            mcp: McpEffect::None,
        }
    }
}

/// What a write did, or with `dry_run` would do, and the pair's settings
/// after it.
pub(in crate::session_pool) struct TierWritten {
    pub(in crate::session_pool) written_to: PathBuf,
    pub(in crate::session_pool) secrets_written_to: Option<PathBuf>,
    /// Only a `dry_run` comes back with refusals.
    pub(in crate::session_pool) rejected: Vec<SettingRejected>,
    pub(in crate::session_pool) revision: String,
    pub(in crate::session_pool) loaded: std::sync::Arc<LoadedSettings>,
    pub(in crate::session_pool) routing: api::RoutingStatus,
}

/// The tier's files after the edit, as read from or written to disk.
pub(in crate::session_pool) struct TierEdited {
    pub(in crate::session_pool) local_changed: bool,
    pub(in crate::session_pool) rejected: Vec<SettingRejected>,
    pub(in crate::session_pool) revision: String,
    /// The pair as the edited files make it, with neither the host's nor the
    /// managed layer.
    pub(in crate::session_pool) loaded: LoadedSettings,
}

pub(in crate::session_pool) enum EditFailure {
    Conflict(String),
    Failed(String),
}

impl From<settings::file::SettingsFileError> for EditFailure {
    fn from(e: settings::file::SettingsFileError) -> Self {
        Self::Failed(e.to_string())
    }
}

/// The tier's two files.
pub(in crate::session_pool) struct TierPaths {
    pub(in crate::session_pool) shared: PathBuf,
    pub(in crate::session_pool) local: PathBuf,
}

impl TierPaths {
    pub(in crate::session_pool) fn in_dir(dir: &Path) -> Self {
        Self {
            shared: dir.join(entry::SETTINGS),
            local: dir.join(entry::SETTINGS_LOCAL),
        }
    }

    /// `tier`'s files of the pair `reader` reads: the very paths its read
    /// takes them from, so the edited contents stand in for them.
    pub(in crate::session_pool) fn of(
        reader: &PairReader,
        tier: api::SettingsTier,
    ) -> Result<Self, PoolError> {
        use base::interface::config_source::Tier;
        let tier = match tier {
            api::SettingsTier::Global => Tier::Global,
            api::SettingsTier::Scene => Tier::Scene,
            api::SettingsTier::Project => Tier::Project,
        };
        reader
            .config_paths()
            .tier_dir(tier)
            .map(|dir| Self::in_dir(&dir))
            .ok_or_else(|| PoolError::Internal("the tier has no directory".into()))
    }
}

/// The refusal of a write some of whose keys cannot be written:
/// `PROJECT_UNTRUSTED` when each is refused only for want of trust.
pub(in crate::session_pool) fn refused(rejected: Vec<SettingRejected>) -> PoolError {
    PoolError::SettingsRejected {
        untrusted_only: rejected.iter().all(|r| r.kind == RejectKind::RequiresTrust),
        rejected,
    }
}

/// Read the tier's files, apply the edit, read the pair as they would make
/// it and judge each changed key; with `dir` held and nothing refused, write
/// them — `settings.local.json` first, so a failure between the two leaves a
/// credential nothing uses rather than a setting without its credential.
/// Judged and written under the same lock, so what is written is what was
/// judged.
#[allow(clippy::too_many_arguments)]
pub(in crate::session_pool) fn edit_tier(
    paths: &TierPaths,
    dir: Option<&settings::file::SettingsDir>,
    ops: &[EditOp],
    if_revision: Option<&str>,
    precondition: Option<Precondition>,
    reader: &PairReader,
    judge_trust: bool,
) -> Result<TierEdited, EditFailure> {
    use settings::file::read_object_and_bytes;
    let (shared_bytes, shared) = read_object_and_bytes(&paths.shared)?;
    let (local_bytes, local) = read_object_and_bytes(&paths.local)?;
    let current = settings::edit::revision(&shared_bytes, &local_bytes);
    if if_revision.is_some_and(|expected| expected != current) {
        return Err(EditFailure::Conflict(current));
    }
    let mut files = TierFiles { shared, local };
    if let Some(check) = precondition {
        check(&files, &paths.shared).map_err(EditFailure::Failed)?;
    }
    let shared_before = files.shared.clone();
    let local_changed = settings::edit::apply(&mut files, ops);
    let loaded = reader.read_as_if(vec![
        (
            paths.shared.clone(),
            serde_json::Value::Object(files.shared.clone()),
        ),
        (
            paths.local.clone(),
            serde_json::Value::Object(files.local.clone()),
        ),
    ]);
    let rejected = judge(&loaded, ops, paths, judge_trust);
    let Some(dir) = dir.filter(|_| rejected.is_empty()) else {
        return Ok(TierEdited {
            local_changed,
            rejected,
            revision: current,
            loaded,
        });
    };
    if local_changed {
        dir.write(entry::SETTINGS_LOCAL, &files.local)?;
    }
    if files.shared != shared_before {
        dir.write(entry::SETTINGS, &files.shared)?;
    }
    let (shared_bytes, _) = read_object_and_bytes(&paths.shared)?;
    let (local_bytes, _) = read_object_and_bytes(&paths.local)?;
    Ok(TierEdited {
        local_changed,
        rejected,
        revision: settings::edit::revision(&shared_bytes, &local_bytes),
        loaded,
    })
}

/// Each changed key the loader left out of the files as edited, and — when
/// the tier is an untrusted project's — each one it withheld. Only what is in
/// a file the op writes counts: a concatenated list is written to
/// `settings.json` alone, so what `settings.local.json` holds beside it is
/// not the op's.
fn judge(
    loaded: &LoadedSettings,
    ops: &[EditOp],
    paths: &TierPaths,
    judge_trust: bool,
) -> Vec<SettingRejected> {
    let (shared, local) = (
        paths.shared.display().to_string(),
        paths.local.display().to_string(),
    );
    let untrusted = judge_trust && loaded.trust().is_untrusted();
    let mut rejected = Vec::new();
    for op in ops.iter().filter(|op| !matches!(op, EditOp::Remove(_))) {
        let path = op.path();
        let writes =
            |origin: &str| origin == shared || (!path.is_concatenated() && origin == local);
        let refused = |key: &str| path.overlaps(&key_of(key));
        if let Some(left_out) = loaded
            .ignored
            .iter()
            .find(|v| writes(&v.origin) && refused(&v.key))
        {
            let reason = if left_out.key == path.dotted() {
                left_out.error.clone()
            } else {
                format!("`{}`: {}", left_out.key, left_out.error)
            };
            rejected.push(SettingRejected::new(
                path.dotted(),
                RejectKind::InvalidValue,
                reason,
            ));
            continue;
        }
        let withheld = loaded
            .withheld()
            .iter()
            .find(|w| untrusted && writes(&w.origin) && refused(&w.key));
        if let Some(withheld) = withheld {
            rejected.push(SettingRejected::new(
                path.dotted(),
                RejectKind::RequiresTrust,
                format!(
                    "the project is not trusted, and `{}` takes effect in a project's own \
                     settings only once it is (project.trust); an untrusted project may only add \
                     and tighten",
                    withheld.key
                ),
            ));
        }
    }
    rejected
}

/// The settings key a loader report names: `permission_rules[0]` and
/// `permission_rules.allow(Bash)` are about `permission_rules` and
/// `permission_rules.allow`.
fn key_of(reported: &str) -> KeyPath {
    let key = reported.split(['(', '[']).next().unwrap_or(reported);
    KeyPath::new(key.split('.').filter(|segment| !segment.is_empty()))
}
