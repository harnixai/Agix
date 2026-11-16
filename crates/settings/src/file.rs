//! Read-modify-write of the settings files the engine changes itself —
//! `project.trust`, `config.setProvider`, `mcp.addServer` / `updateServer`
//! and the "always allow" answer to a permission prompt.
//!
//! [`SettingsDir::lock`] holds one tier's directory against every other
//! writer: other threads of this process, and, on unix, other processes
//! through `flock` on the directory. While it is held, [`SettingsDir::read`] and
//! [`SettingsDir::write`] act on the files in it. A write goes to a temporary
//! file in the same directory, is flushed, and is renamed over the old one,
//! so a reader sees the old file or the new one and never part of one.
//! `settings.local.json` holds credentials and is always written `0600`; any
//! other file keeps the mode it had.
//!
//! A settings file that is a symbolic link is refused, for reading and for
//! writing alike. In a project tier the link is repository content: writing
//! through it would let a repository pick which of the user's files is
//! rewritten, and replacing it would quietly undo a user's own dotfile link.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Map, Value};

use base::dir_lock::{DirLock, LockTimeout, DIR_LOCK_WAIT};
use base::interface::permission::{GrantMatch, RuleFile};
use base::paths::SETTINGS_LOCAL_FILE;

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SettingsFileError {
    #[error("{}: {source}", path.display())]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("the settings directory {} is locked by another process; nothing was written", dir.display())]
    Locked { dir: PathBuf },
    #[error("existing {} is not valid JSON, refusing to overwrite it: {source}", path.display())]
    NotJson {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("existing {} is not a JSON object at the top level, refusing to overwrite it", path.display())]
    NotObject { path: PathBuf },
    #[error("{} is a symbolic link; settings are not written through one, edit the file it points to instead", path.display())]
    Symlink { path: PathBuf },
}

impl SettingsFileError {
    fn io(path: &Path) -> impl FnOnce(std::io::Error) -> Self + '_ {
        move |source| Self::Io {
            path: path.to_path_buf(),
            source,
        }
    }
}

/// One tier's settings directory, held for a read-modify-write.
pub struct SettingsDir {
    lock: DirLock,
}

impl SettingsDir {
    /// Create `dir` if it is missing and wait, up to [`DIR_LOCK_WAIT`], until
    /// no one else holds it.
    pub fn lock(dir: &Path) -> Result<Self, SettingsFileError> {
        Self::lock_within(dir, DIR_LOCK_WAIT)
    }

    fn lock_within(dir: &Path, wait: Duration) -> Result<Self, SettingsFileError> {
        match DirLock::acquire_within(dir, wait) {
            Ok(lock) => Ok(Self { lock }),
            Err(e) => match LockTimeout::of(&e) {
                Some(timeout) => Err(SettingsFileError::Locked {
                    dir: timeout.dir.clone(),
                }),
                None => Err(SettingsFileError::io(dir)(e)),
            },
        }
    }

    pub fn path(&self, file: &str) -> PathBuf {
        self.lock.dir().join(file)
    }

    /// The file's top-level object; empty when it does not exist or has
    /// nothing in it.
    pub fn read(&self, file: &str) -> Result<Map<String, Value>, SettingsFileError> {
        read_object(&self.path(file))
    }

    pub fn write(&self, file: &str, value: &Map<String, Value>) -> Result<(), SettingsFileError> {
        let path = self.path(file);
        refuse_symlink(&path)?;
        let body =
            serde_json::to_string_pretty(value).map_err(|source| SettingsFileError::NotJson {
                path: path.clone(),
                source,
            })?;
        let opts = base::fs::WriteOpts {
            mode: mode_for(file, &path),
            fsync: true,
        };
        if let Err(source) = base::fs::write_atomic(&path, body.as_bytes(), opts) {
            return Err(SettingsFileError::Io { path, source });
        }
        self.lock.sync();
        if file == SETTINGS_LOCAL_FILE || file == base::paths::entry::MCP_LOCAL {
            base::paths::ensure_local_data_gitignore(self.lock.dir());
        }
        Ok(())
    }
}

/// What [`SettingsDir::read`] reads, without holding the directory or creating
/// it: for a caller that only looks at what a write would start from.
pub fn read_object(path: &Path) -> Result<Map<String, Value>, SettingsFileError> {
    refuse_symlink(path)?;
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Map::new()),
        Err(source) => {
            return Err(SettingsFileError::Io {
                path: path.to_path_buf(),
                source,
            })
        }
    };
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(Map::new());
    }
    let parsed: Value =
        serde_json::from_slice(&bytes).map_err(|source| SettingsFileError::NotJson {
            path: path.to_path_buf(),
            source,
        })?;
    match parsed {
        Value::Object(fields) => Ok(fields),
        _ => Err(SettingsFileError::NotObject {
            path: path.to_path_buf(),
        }),
    }
}

/// `None` keeps what the system gives a new file.
fn mode_for(file: &str, path: &Path) -> Option<u32> {
    if file == SETTINGS_LOCAL_FILE {
        return Some(0o600);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .ok()
            .map(|m| m.permissions().mode() & 0o7777)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

fn refuse_symlink(path: &Path) -> Result<(), SettingsFileError> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => Err(SettingsFileError::Symlink {
            path: path.to_path_buf(),
        }),
        _ => Ok(()),
    }
}

/// Which behavior bucket to append to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum AppendTarget {
    Allow,
    Deny,
    Ask,
}

impl AppendTarget {
    fn action(&self) -> &'static str {
        match self {
            AppendTarget::Allow => "allow",
            AppendTarget::Deny => "deny",
            AppendTarget::Ask => "ask",
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AppendError {
    #[error(transparent)]
    File(#[from] SettingsFileError),
    #[error("{} has no file name", .0.display())]
    NoFileName(std::path::PathBuf),
    #[error("permission_rules in the settings file is not a list")]
    NotAList,
}

/// Append `rule_string` to `permission_rules` in the given settings.json
/// file, as `{"tool": <rule_string>, "action": <target>}`. If the file
/// doesn't exist, it's created with `{}` first. If an identical entry is
/// already present, the file is left untouched.
///
/// The shape here is the one `base::settings::Settings`
/// deserializes — writing anything else produces a file `LoadedSettings::load`
/// silently drops as an unknown key.
///
/// Returns `Ok(true)` if the file was modified, `Ok(false)` if the rule was
/// already present.
pub fn append_permission_rule(
    settings_path: &Path,
    target: AppendTarget,
    rule_string: &str,
) -> Result<bool, AppendError> {
    append(settings_path, target, rule_string, GrantMatch::Pattern)
}

/// [`append_permission_rule`] for an allow whose content is matched only by
/// the same text, written with `"match": "exact"`.
pub fn append_exact_allow(settings_path: &Path, rule_string: &str) -> Result<bool, AppendError> {
    append(
        settings_path,
        AppendTarget::Allow,
        rule_string,
        GrantMatch::Exact,
    )
}

fn append(
    settings_path: &Path,
    target: AppendTarget,
    rule_string: &str,
    matching: GrantMatch,
) -> Result<bool, AppendError> {
    let file = settings_path
        .file_name()
        .and_then(|f| f.to_str())
        .ok_or_else(|| AppendError::NoFileName(settings_path.to_path_buf()))?;
    let dir = match settings_path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    let tier = SettingsDir::lock(dir)?;
    let mut value = tier.read(file)?;

    let arr = value
        .entry("permission_rules".to_string())
        .or_insert_with(|| serde_json::Value::Array(Vec::new()));
    let arr = arr.as_array_mut().ok_or(AppendError::NotAList)?;

    let action = target.action();
    let mut entry = serde_json::json!({ "tool": rule_string, "action": action });
    if !matching.is_pattern() {
        entry["match"] = serde_json::to_value(matching).unwrap_or_default();
    }
    let already = arr.iter().any(|v| {
        ["tool", "action", "match"]
            .iter()
            .all(|key| v.get(key) == entry.get(key))
    });
    if already {
        return Ok(false);
    }
    arr.push(entry);

    tier.write(file, &value)?;
    Ok(true)
}

/// The local settings file as the permission engine's [`RuleFile`], or the
/// reason a rule is not written there.
#[derive(Debug, Clone)]
pub struct LocalRules {
    path: PathBuf,
    refusal: Option<String>,
}

impl LocalRules {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            refusal: None,
        }
    }

    pub fn refusing(path: PathBuf, reason: impl Into<String>) -> Self {
        Self {
            path,
            refusal: Some(reason.into()),
        }
    }
}

impl RuleFile for LocalRules {
    fn path(&self) -> &Path {
        &self.path
    }

    fn append_allow(&self, rule: &str, matching: GrantMatch) -> Result<bool, String> {
        if let Some(reason) = &self.refusal {
            return Err(reason.clone());
        }
        append(&self.path, AppendTarget::Allow, rule, matching).map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base::paths::SETTINGS_FILE;
    use serde_json::json;
    use std::fs;
    use tempfile::TempDir;

    fn obj(value: Value) -> Map<String, Value> {
        match value {
            Value::Object(fields) => fields,
            _ => unreachable!(),
        }
    }

    #[test]
    fn a_held_directory_is_refused_as_locked_and_left_alone() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join(SETTINGS_FILE), "{}").unwrap();
        let held = SettingsDir::lock(dir.path()).unwrap();
        let err = SettingsDir::lock_within(dir.path(), Duration::from_millis(100))
            .err()
            .expect("held");
        assert!(
            matches!(&err, SettingsFileError::Locked { dir: d } if *d == dir.path().canonicalize().unwrap()),
            "{err}"
        );
        assert!(err.to_string().contains("nothing was written"));
        drop(held);
        SettingsDir::lock_within(dir.path(), Duration::from_secs(5)).unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join(SETTINGS_FILE)).unwrap(),
            "{}"
        );
    }

    #[test]
    fn a_missing_or_empty_file_reads_as_an_empty_object() {
        let dir = tempfile::tempdir().unwrap();
        let tier = SettingsDir::lock(&dir.path().join("new")).unwrap();
        assert_eq!(Value::Object(tier.read(SETTINGS_FILE).unwrap()), json!({}));
        std::fs::write(tier.path(SETTINGS_FILE), "  \n").unwrap();
        assert_eq!(Value::Object(tier.read(SETTINGS_FILE).unwrap()), json!({}));
    }

    #[test]
    fn what_is_not_a_settings_object_is_refused_not_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let tier = SettingsDir::lock(dir.path()).unwrap();
        std::fs::write(tier.path(SETTINGS_FILE), "{oops").unwrap();
        assert!(matches!(
            tier.read(SETTINGS_FILE),
            Err(SettingsFileError::NotJson { .. })
        ));
        std::fs::write(tier.path(SETTINGS_FILE), "[]").unwrap();
        assert!(matches!(
            tier.read(SETTINGS_FILE),
            Err(SettingsFileError::NotObject { .. })
        ));
    }

    /// The first credentials written into a directory bring a `.gitignore`
    /// that keeps them, and the engine's other local files, out of the
    /// repository the directory sits in. One already there is the user's.
    #[test]
    fn writing_credentials_keeps_them_out_of_git() {
        let dir = tempfile::tempdir().unwrap();
        let tier = SettingsDir::lock(dir.path()).unwrap();
        tier.write(SETTINGS_LOCAL_FILE, &obj(json!({"providers": {}})))
            .unwrap();
        let ignored = std::fs::read_to_string(dir.path().join(".gitignore")).unwrap();
        for line in ["settings.local.json", "mcp.local.json", "telemetry/"] {
            assert!(ignored.lines().any(|l| l == line), "{ignored}");
        }

        let mine = tempfile::tempdir().unwrap();
        std::fs::write(mine.path().join(".gitignore"), "mine\n").unwrap();
        let tier = SettingsDir::lock(mine.path()).unwrap();
        tier.write(SETTINGS_LOCAL_FILE, &obj(json!({}))).unwrap();
        let kept = std::fs::read_to_string(mine.path().join(".gitignore")).unwrap();
        assert!(kept.starts_with("mine\n"), "{kept}");
        assert!(kept.lines().any(|l| l == "settings.local.json"), "{kept}");
    }

    #[test]
    fn a_write_replaces_the_file_whole_and_leaves_nothing_behind() {
        let dir = tempfile::tempdir().unwrap();
        let tier = SettingsDir::lock(dir.path()).unwrap();
        tier.write(SETTINGS_FILE, &obj(json!({"language": "en"})))
            .unwrap();
        tier.write(SETTINGS_FILE, &obj(json!({"language": "zh"})))
            .unwrap();
        assert_eq!(
            Value::Object(tier.read(SETTINGS_FILE).unwrap()),
            json!({"language": "zh"})
        );
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, [SETTINGS_FILE]);
    }

    #[cfg(unix)]
    #[test]
    fn the_local_file_is_always_private_and_the_shared_one_keeps_its_mode() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let tier = SettingsDir::lock(dir.path()).unwrap();
        let mode = |file: &str| {
            std::fs::metadata(tier.path(file))
                .unwrap()
                .permissions()
                .mode()
                & 0o777
        };

        std::fs::write(tier.path(SETTINGS_LOCAL_FILE), "{}").unwrap();
        std::fs::set_permissions(
            tier.path(SETTINGS_LOCAL_FILE),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        tier.write(SETTINGS_LOCAL_FILE, &obj(json!({"a": 1})))
            .unwrap();
        assert_eq!(mode(SETTINGS_LOCAL_FILE), 0o600);

        std::fs::write(tier.path(SETTINGS_FILE), "{}").unwrap();
        std::fs::set_permissions(
            tier.path(SETTINGS_FILE),
            std::fs::Permissions::from_mode(0o640),
        )
        .unwrap();
        tier.write(SETTINGS_FILE, &obj(json!({"a": 1}))).unwrap();
        assert_eq!(mode(SETTINGS_FILE), 0o640);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_settings_file_is_neither_read_nor_written() {
        let dir = tempfile::tempdir().unwrap();
        let elsewhere = dir.path().join("elsewhere.json");
        std::fs::write(&elsewhere, r#"{"keep": true}"#).unwrap();
        let tier_dir = dir.path().join(".agix");
        std::fs::create_dir_all(&tier_dir).unwrap();
        std::os::unix::fs::symlink(&elsewhere, tier_dir.join(SETTINGS_LOCAL_FILE)).unwrap();

        let tier = SettingsDir::lock(&tier_dir).unwrap();
        assert!(matches!(
            tier.read(SETTINGS_LOCAL_FILE),
            Err(SettingsFileError::Symlink { .. })
        ));
        assert!(matches!(
            tier.write(SETTINGS_LOCAL_FILE, &obj(json!({"api_key": "k"}))),
            Err(SettingsFileError::Symlink { .. })
        ));
        assert_eq!(
            std::fs::read_to_string(&elsewhere).unwrap(),
            r#"{"keep": true}"#
        );
        assert!(
            std::fs::symlink_metadata(tier_dir.join(SETTINGS_LOCAL_FILE))
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn concurrent_edits_of_one_directory_do_not_lose_updates() {
        let dir = tempfile::tempdir().unwrap();
        let n = 16;
        std::thread::scope(|scope| {
            for i in 0..n {
                let dir = dir.path();
                scope.spawn(move || {
                    let tier = SettingsDir::lock(dir).unwrap();
                    let mut value = tier.read(SETTINGS_LOCAL_FILE).unwrap();
                    value.insert(format!("k{i}"), json!(i));
                    tier.write(SETTINGS_LOCAL_FILE, &value).unwrap();
                });
            }
        });
        let tier = SettingsDir::lock(dir.path()).unwrap();
        assert_eq!(tier.read(SETTINGS_LOCAL_FILE).unwrap().len(), n);
    }

    /// N threads appending distinct rules to the *same* settings file
    /// concurrently must all survive: unserialized, a thread reads the file
    /// before an earlier thread's rename lands, then overwrites it on its own
    /// write.
    #[test]
    fn concurrent_appends_to_the_same_file_do_not_lose_updates() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("settings.json");
        let n = 12;

        std::thread::scope(|scope| {
            for i in 0..n {
                let p = &p;
                scope.spawn(move || {
                    append_permission_rule(p, AppendTarget::Allow, &format!("Bash(cmd{i})"))
                        .unwrap();
                });
            }
        });

        let v: serde_json::Value = serde_json::from_slice(&fs::read(&p).unwrap()).unwrap();
        let arr = v["permission_rules"].as_array().unwrap();
        assert_eq!(
            arr.len(),
            n,
            "expected all {n} concurrently-appended rules to survive, got {arr:?}"
        );
    }

    /// The written shape must be the one the loader actually reads — a file
    /// whose rules land under a key the loader doesn't know is
    /// indistinguishable from an empty one once loaded.
    #[test]
    fn appended_rules_round_trip_through_settings_load() {
        let dir = TempDir::new().unwrap();
        let local = dir.path().join(base::paths::SETTINGS_LOCAL_FILE);
        append_permission_rule(&local, AppendTarget::Allow, "Bash(git status)").unwrap();
        append_permission_rule(&local, AppendTarget::Deny, "Bash(rm -rf:*)").unwrap();

        let empty = TempDir::new().unwrap();
        let root = dir.path().parent().unwrap().display().to_string();
        std::fs::write(
            empty.path().join(base::paths::SETTINGS_FILE),
            serde_json::json!({"project_trust": {"trusted": [root]}}).to_string(),
        )
        .unwrap();
        let settings = crate::LoadedSettings::load(
            empty.path().to_path_buf(),
            empty.path().to_path_buf(),
            dir.path().to_path_buf(),
            "code",
            "m",
            &crate::section::Sections::none(),
            Default::default(),
        );

        let loaded: Vec<_> = settings
            .rules
            .iter()
            .filter(|r| r.machine_local)
            .map(|r| (r.rule.tool.as_str(), r.rule.action))
            .collect();
        assert_eq!(
            loaded,
            vec![
                ("Bash(git status)", base::permission::RuleBehavior::Allow),
                ("Bash(rm -rf:*)", base::permission::RuleBehavior::Deny),
            ]
        );
    }

    #[test]
    fn append_creates_file_if_absent() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("subdir").join("settings.json");
        let modified = append_permission_rule(&p, AppendTarget::Allow, "Bash(ls)").unwrap();
        assert!(modified);
        let v: serde_json::Value = serde_json::from_slice(&fs::read(&p).unwrap()).unwrap();
        let arr = v["permission_rules"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(
            arr[0],
            serde_json::json!({"tool":"Bash(ls)","action":"allow"})
        );
    }

    #[test]
    fn append_idempotent_when_rule_present() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("settings.json");
        fs::write(
            &p,
            r#"{"permission_rules":[{"tool":"Bash(git status)","action":"allow"}]}"#,
        )
        .unwrap();
        let modified = append_permission_rule(&p, AppendTarget::Allow, "Bash(git status)").unwrap();
        assert!(!modified);
    }

    /// Same tool, different action is a different rule — the idempotency
    /// check keys on both.
    #[test]
    fn append_same_tool_with_a_different_action_is_not_a_duplicate() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("settings.json");
        append_permission_rule(&p, AppendTarget::Allow, "Bash(ls)").unwrap();
        let modified = append_permission_rule(&p, AppendTarget::Deny, "Bash(ls)").unwrap();
        assert!(modified);
        let v: serde_json::Value = serde_json::from_slice(&fs::read(&p).unwrap()).unwrap();
        assert_eq!(v["permission_rules"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn append_preserves_other_keys() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("settings.json");
        fs::write(
            &p,
            r#"{"model":"x","weird":42,"permission_rules":[{"tool":"Bash(rm:*)","action":"deny"}]}"#,
        )
        .unwrap();
        append_permission_rule(&p, AppendTarget::Allow, "Bash(ls)").unwrap();
        let v: serde_json::Value = serde_json::from_slice(&fs::read(&p).unwrap()).unwrap();
        assert_eq!(v["model"], "x");
        assert_eq!(v["weird"], 42);
        assert_eq!(
            v["permission_rules"][0],
            serde_json::json!({"tool":"Bash(rm:*)","action":"deny"})
        );
        assert_eq!(
            v["permission_rules"][1],
            serde_json::json!({"tool":"Bash(ls)","action":"allow"})
        );
    }

    #[test]
    fn append_can_target_deny_or_ask() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("settings.json");
        append_permission_rule(&p, AppendTarget::Deny, "Bash(rm -rf:*)").unwrap();
        append_permission_rule(&p, AppendTarget::Ask, "Bash(git push:*)").unwrap();
        let v: serde_json::Value = serde_json::from_slice(&fs::read(&p).unwrap()).unwrap();
        assert_eq!(
            v["permission_rules"][0],
            serde_json::json!({"tool":"Bash(rm -rf:*)","action":"deny"})
        );
        assert_eq!(
            v["permission_rules"][1],
            serde_json::json!({"tool":"Bash(git push:*)","action":"ask"})
        );
    }

    /// `settings.local.json` holds provider keys and MCP headers; answering
    /// "always allow" must not leave it readable by everyone.
    #[cfg(unix)]
    #[test]
    fn the_local_settings_file_stays_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new().unwrap();
        let p = dir.path().join(base::paths::SETTINGS_LOCAL_FILE);
        fs::write(&p, r#"{"providers":{"x":{"api_key":"sk"}}}"#).unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o600)).unwrap();

        append_permission_rule(&p, AppendTarget::Allow, "Bash(ls)").unwrap();

        assert_eq!(
            fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let fresh = dir
            .path()
            .join("fresh")
            .join(base::paths::SETTINGS_LOCAL_FILE);
        append_permission_rule(&fresh, AppendTarget::Allow, "Bash(ls)").unwrap();
        assert_eq!(
            fs::metadata(&fresh).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_settings_file_is_not_written_through() {
        let dir = TempDir::new().unwrap();
        let target = dir.path().join("dotfiles.json");
        fs::write(&target, "{}").unwrap();
        let p = dir.path().join("settings.local.json");
        std::os::unix::fs::symlink(&target, &p).unwrap();

        assert!(append_permission_rule(&p, AppendTarget::Allow, "Bash(ls)").is_err());
        assert_eq!(fs::read_to_string(&target).unwrap(), "{}");
    }

    /// An exact allow is written with `"match": "exact"`, once, and apart
    /// from a pattern rule of the same text.
    #[test]
    fn an_exact_allow_is_written_with_its_match() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("settings.local.json");
        assert!(append_permission_rule(&p, AppendTarget::Allow, "Bash(ls *)").unwrap());
        assert!(append_exact_allow(&p, "Bash(ls *)").unwrap());
        assert!(!append_exact_allow(&p, "Bash(ls *)").unwrap());
        let written: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(
            written["permission_rules"],
            serde_json::json!([
                {"tool": "Bash(ls *)", "action": "allow"},
                {"tool": "Bash(ls *)", "action": "allow", "match": "exact"},
            ])
        );
    }
}
