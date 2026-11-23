//! Test root of the settings crate: shared fixtures for the topic files beside it.

use super::*;
use crate::section::Sections;
use base::paths::{SETTINGS_FILE, SETTINGS_LOCAL_FILE};
use base::permission::{LoadedRule, PermissionMode, RuleBehavior};

mod diagnostics;
mod managed;
mod mcp_servers;
mod permissions;
mod project_trust;
mod providers;
mod sources;
mod tiers;
mod values_and_merge;

fn shared_rules(s: &LoadedSettings) -> Vec<&LoadedRule> {
    s.rules.iter().filter(|r| !r.machine_local).collect()
}

fn local_rules(s: &LoadedSettings) -> Vec<&LoadedRule> {
    s.rules.iter().filter(|r| r.machine_local).collect()
}

fn write_settings(dir: &std::path::Path, content: &str) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join(SETTINGS_FILE), content).unwrap();
}

fn write_local_settings(dir: &std::path::Path, content: &str) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join(SETTINGS_LOCAL_FILE), content).unwrap();
}

/// Have the global tier in `global` trust the project whose `.agix` is
/// `project`, keeping whatever else that tier's `settings.json` says.
fn trust_project(global: &std::path::Path, project: &std::path::Path) {
    let path = global.join(SETTINGS_FILE);
    let mut settings: serde_json::Value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    let root = project.parent().unwrap().display().to_string();
    settings["app"]["trusted_projects"] = serde_json::json!([root]);
    write_settings(global, &settings.to_string());
}

fn load_global(file: &str) -> LoadedSettings {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("settings.json"), file).unwrap();
    LoadedSettings::load(
        dir.path().to_path_buf(),
        dir.path().join("scene"),
        dir.path().join("project"),
        "code",
        "m",
        &Sections::none(),
        Default::default(),
    )
}

fn ignored_keys(settings: &LoadedSettings) -> Vec<&str> {
    settings.ignored.iter().map(|v| v.key.as_str()).collect()
}
