//! `config.set` 收哪些键：有专门方法的键族指过去，可执行配置不收，只从全局层读的键
//! 只能写全局层，schema 里没有的键不收。值本身放不放得进、未受信项目收不收，由写入核心
//! 按加载器判。

use settings::edit::KeyPath;
use settings::{KeyKind, SchemaKeys};

use crate::session_pool::api::{RejectKind, SettingRejected, SettingsTier};

/// The most keys one `config.set` takes.
pub(in crate::session_pool) const CONFIG_SET_MAX_KEYS: usize = 256;

enum Refusal {
    UseMethod(&'static str, &'static str),
    NotWritable(&'static str),
}

/// Key families `config.set` does not write, dotted, with `*` for one map
/// key: a key at, above or beneath one of them is refused, for the reason of
/// the family it names exactly, else of the first it overlaps.
const CONFIG_SET_REFUSES: &[(&str, Refusal)] = &[
    (
        "mcp.servers",
        Refusal::UseMethod(
            "mcp.addServer",
            "a server is admitted by the MCP policy and connected or disconnected as it is \
             written; mcp.addServer / mcp.updateServer write one",
        ),
    ),
    (
        "mcp.servers.*.enabled",
        Refusal::UseMethod(
            "mcp.setEnabled",
            "turning a server off or on disconnects or connects it as it is written",
        ),
    ),
    (
        "app.trusted_projects",
        Refusal::UseMethod(
            "project.trust",
            "trust is recorded for one project root at a time, in the global tier's \
             settings.local.json",
        ),
    ),
    (
        "app.trust_all_projects",
        Refusal::UseMethod(
            "project.trust",
            "trust is recorded for one project root at a time, in the global tier's \
             settings.local.json",
        ),
    ),
    (
        "hooks",
        Refusal::NotWritable(
            "hooks run commands; they are configured in the settings files themselves, not \
             over RPC",
        ),
    ),
    (
        "scripts",
        Refusal::NotWritable(
            "scripts run code; they are configured in the settings files themselves, not over \
             RPC",
        ),
    ),
];

/// Why `config.set` does not write `value` (`None`: remove) at `path` of
/// `tier`, if it does not.
pub(in crate::session_pool) fn refusal(
    path: &KeyPath,
    value: Option<&serde_json::Value>,
    tier: SettingsTier,
    keys: &SchemaKeys,
) -> Option<SettingRejected> {
    let segments = path.segments();
    let dotted = path.dotted();
    let overlaps = |family: &str| {
        family
            .split('.')
            .zip(&segments)
            .all(|(f, s)| f == "*" || f == *s)
    };
    let exactly = |family: &str| family.split('.').count() == segments.len() && overlaps(family);
    let refused = CONFIG_SET_REFUSES
        .iter()
        .find(|(family, _)| exactly(family))
        .or_else(|| {
            CONFIG_SET_REFUSES
                .iter()
                .find(|(family, _)| overlaps(family))
        });
    if let Some((_, refusal)) = refused {
        return Some(match refusal {
            Refusal::UseMethod(method, reason) => SettingRejected {
                method: Some(method.to_string()),
                ..SettingRejected::new(dotted, RejectKind::UseMethod, *reason)
            },
            Refusal::NotWritable(reason) => {
                SettingRejected::new(dotted, RejectKind::NotWritable, *reason)
            }
        });
    }
    let value = value?;
    let unknown = match keys.kind(&segments) {
        KeyKind::Unknown => vec![dotted.clone()],
        _ => keys.unknown_in(&segments, value),
    };
    if !unknown.is_empty() {
        let renamed: Vec<String> = unknown
            .iter()
            .filter_map(|key| {
                settings::renamed::renamed_to(key).map(|new| format!("`{key}` is now `{new}`"))
            })
            .collect();
        let mut reason = format!("no setting reads {}", quoted(&unknown));
        if !renamed.is_empty() {
            reason = format!("{reason} ({})", renamed.join("; "));
        }
        return Some(SettingRejected::new(dotted, RejectKind::UnknownKey, reason));
    }
    if tier != SettingsTier::Global && settings::GLOBAL_ONLY_KEYS.contains(&segments[0]) {
        return Some(SettingRejected::new(
            dotted,
            RejectKind::NotAtTier,
            format!("`{}` is read from the global tier only", segments[0]),
        ));
    }
    None
}

fn quoted(keys: &[String]) -> String {
    keys.iter()
        .map(|k| format!("`{k}`"))
        .collect::<Vec<_>>()
        .join(", ")
}
