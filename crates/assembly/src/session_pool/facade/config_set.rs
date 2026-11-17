//! `config.set`：把"键路径 → 值"翻成对一层的改动，先按键表拒掉不收的键，再交给按层
//! 写设置的核心；答复带每个键写完之后在这一对里的生效值。

use super::listing::effective_settings_json;
use crate::session_pool::*;
use settings::edit::{EditOp, KeyPath};

impl SessionPool {
    /// `config.set` — write `change.changes` into `change.tier` of the pair
    /// `(project_root, scene_id)`, all of them or, when any is refused, none.
    pub async fn set_settings(
        &self,
        project_root: Option<&Path>,
        scene_id: &str,
        change: api::SettingsChange,
    ) -> Result<api::SettingsWritten, PoolError> {
        if change.changes.is_empty() {
            return Err(PoolError::InvalidRequest(
                "changes must name at least one key".into(),
            ));
        }
        if change.changes.len() > CONFIG_SET_MAX_KEYS {
            return Err(PoolError::InvalidRequest(format!(
                "changes names {} keys; one write takes at most {CONFIG_SET_MAX_KEYS}",
                change.changes.len()
            )));
        }
        let (ops, mut rejected) = changes_to_ops(change.tier, change.changes);
        let refused_already = !rejected.is_empty();
        let touches_connections = ops.iter().any(|op| {
            ["sandbox.subprocess_env", "network"]
                .iter()
                .any(|key| op.path().overlaps(&KeyPath::new(key.split('.'))))
        });
        let edit = TierEdit {
            if_revision: change.if_revision,
            dry_run: change.dry_run || refused_already,
            mcp: if touches_connections {
                McpEffect::Full {
                    connect_pair: false,
                }
            } else {
                McpEffect::None
            },
            ..TierEdit::new(change.tier, ops.clone())
        };
        let written = self
            .write_settings_tier(project_root, scene_id, edit)
            .await?;
        rejected.extend(written.rejected);
        rejected.sort_by(|a, b| a.key.cmp(&b.key));
        if !rejected.is_empty() && !change.dry_run {
            return Err(refused(rejected));
        }
        let (changed, effective, overridden) = if rejected.is_empty() {
            effective_values(&written.loaded, &ops)
        } else {
            Default::default()
        };
        Ok(api::SettingsWritten {
            tier: change.tier,
            dry_run: change.dry_run,
            written_to: written.written_to.display().to_string(),
            secrets_written_to: written.secrets_written_to.map(|p| p.display().to_string()),
            changed,
            rejected,
            effective,
            overridden,
            revision: written.revision,
            warnings: written.warnings,
            reloaded: settings_reloaded(&written.loaded.values, written.routing),
        })
    }
}

/// Each change as an op, or why `config.set` does not take it.
fn changes_to_ops(
    tier: api::SettingsTier,
    changes: serde_json::Map<String, serde_json::Value>,
) -> (Vec<EditOp>, Vec<api::SettingRejected>) {
    let keys = settings::SchemaKeys::of(&crate::settings_sections::all());
    let mut rejected = Vec::new();
    let mut parsed = Vec::new();
    for (dotted, value) in changes {
        match KeyPath::parse(&dotted) {
            Ok(path) => parsed.push((path, value)),
            Err(reason) => {
                rejected.push(api::SettingRejected::new(
                    dotted,
                    api::RejectKind::BadPath,
                    reason,
                ));
            }
        }
    }
    let mut ops = Vec::new();
    for (i, (path, value)) in parsed.iter().enumerate() {
        if let Some((other, _)) = parsed
            .iter()
            .enumerate()
            .find(|(j, (other, _))| *j != i && path.overlaps(other))
            .map(|(_, entry)| entry)
        {
            rejected.push(api::SettingRejected::new(
                path.dotted(),
                api::RejectKind::BadPath,
                format!(
                    "overlaps `{}` in the same write; write one of them",
                    other.dotted()
                ),
            ));
            continue;
        }
        let value = (!value.is_null()).then_some(value);
        if let Some(refused) = refusal(path, value, tier, &keys) {
            rejected.push(refused);
            continue;
        }
        ops.push(match value {
            Some(value) => EditOp::Set(path.clone(), value.clone()),
            None => EditOp::Remove(path.clone()),
        });
    }
    (ops, rejected)
}

/// The keys written, each one's value in `loaded` (credentials redacted),
/// and those whose value there is not the one written.
fn effective_values(
    loaded: &LoadedSettings,
    ops: &[EditOp],
) -> (
    Vec<String>,
    serde_json::Map<String, serde_json::Value>,
    Vec<String>,
) {
    let merged = effective_settings_json(loaded);
    let mut shown = merged.clone();
    redact_settings_secrets(&mut shown);
    let at = |root: &serde_json::Value, path: &KeyPath| {
        path.segments()
            .iter()
            .try_fold(root, |node, segment| node.get(*segment))
            .cloned()
            .unwrap_or(serde_json::Value::Null)
    };
    let mut changed = Vec::new();
    let mut effective = serde_json::Map::new();
    let mut overridden = Vec::new();
    for op in ops {
        let path = op.path();
        changed.push(path.dotted());
        effective.insert(path.dotted(), at(&shown, path));
        if let EditOp::Set(_, written) = op {
            let leaf = !written.is_object() && path.dotted() != "permission_rules";
            if leaf && at(&merged, path) != *written {
                overridden.push(path.dotted());
            }
        }
    }
    (changed, effective, overridden)
}
