//! Trusting a case's projects from its global tier, where `project.trust`
//! would record the host's answer. For cases about what a trusted project's
//! own configuration does, not about trust.

use std::path::Path;

/// Trust `root` and every project under it, keeping whatever else the
/// global `settings.local.json` in `global` says.
pub fn trust_projects_under(global: &Path, root: &Path) {
    let path = global.join("settings.local.json");
    let mut local: serde_json::Value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    let trusted = local["project_trust"]["trusted"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .chain([serde_json::json!(root), serde_json::json!(root.join("**"))])
        .collect::<Vec<_>>();
    local["project_trust"]["trusted"] = serde_json::Value::Array(trusted);
    std::fs::create_dir_all(global).unwrap();
    std::fs::write(&path, local.to_string()).unwrap();
}
