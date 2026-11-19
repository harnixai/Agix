//! The functions a package exposes to the host (`[[function]]`): entries
//! checked like a command's, and an input schema read and compiled at
//! install, so a call never finds out the package was broken.

use crate::manifest::{FunctionDef, Plugin, PluginError};
use base::command::PluginFunction;
use std::path::Path;

/// The most an input schema file may hold.
pub const MAX_SCHEMA_BYTES: u64 = 64 * 1024;

/// Every function `plugin` declares. Read at install and again at
/// discovery, like [`crate::commands::declared_commands`].
pub fn declared_functions(plugin: &Plugin) -> Result<Vec<PluginFunction>, PluginError> {
    plugin
        .manifest
        .function
        .iter()
        .map(|f| {
            let what = format!("function `{}`", f.name);
            Ok(PluginFunction {
                name: f.name.clone(),
                plugin: plugin.name().to_string(),
                plugin_version: plugin.manifest.plugin.version.clone(),
                source: plugin.source(),
                description: f.description.clone(),
                root: plugin.root.clone(),
                file: crate::commands::entry_file(plugin, &what, f.file())?,
                function: f.function().to_string(),
                writes: f.writes.clone(),
                input_schema: input_schema(plugin, f, &what)?,
                uses: plugin.manifest.uses.clone(),
                bytecode_dir: plugin.bytecode_dir().map(Path::to_path_buf),
            })
        })
        .collect()
}

fn input_schema(
    plugin: &Plugin,
    f: &FunctionDef,
    what: &str,
) -> Result<Option<serde_json::Value>, PluginError> {
    let Some(rel) = &f.input_schema else {
        return Ok(None);
    };
    let refused =
        |why: String| PluginError::Schema(format!("{what} input_schema `{}` {why}", rel.display()));
    let file = plugin.path(rel).map_err(|e| refused(e.to_string()))?;
    let size = std::fs::metadata(&file)?.len();
    if size > MAX_SCHEMA_BYTES {
        return Err(refused(format!(
            "is {size} bytes, over the {MAX_SCHEMA_BYTES} allowed"
        )));
    }
    let schema: serde_json::Value = serde_json::from_slice(&std::fs::read(&file)?)
        .map_err(|e| refused(format!("is not JSON: {e}")))?;
    jsonschema::validator_for(&schema)
        .map_err(|e| refused(format!("is not a JSON Schema: {e}")))?;
    Ok(Some(schema))
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEAD: &str = "[plugin]\nname = \"xflow\"\nversion = \"1.0.0\"\napi_version = \"0.1\"\n";

    fn load(root: &Path, body: &str) -> Result<Vec<PluginFunction>, PluginError> {
        std::fs::write(
            root.join("plugin.toml"),
            format!("uses = [\"codix\"]\n{HEAD}{body}"),
        )
        .unwrap();
        let plugin = Plugin::load(root, &root.join("plugin.toml"), &Default::default())?;
        declared_functions(&plugin)
    }

    const CHECK: &str = "\n[[function]]\nname = \"check\"\nentry = \"xflow.js:check\"\n\
        description = \"gate check\"\nwrites = [\"reports/**\"]\ninput_schema = \"check.json\"\n";

    #[test]
    fn a_function_carries_its_package_entry_and_compiled_schema() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir
            .path()
            .join(base::paths::entry::CACHE)
            .join("xflow/1.0.0");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("xflow.js"), "async function check(input, ctx) {}").unwrap();
        std::fs::write(root.join("check.json"), r#"{"type": "object"}"#).unwrap();
        let functions = load(&root, CHECK).unwrap();
        let f = &functions[0];
        assert_eq!(f.call_name(), "xflow.check");
        assert_eq!(
            (f.function.as_str(), f.plugin_version.as_str()),
            ("check", "1.0.0")
        );
        assert!(f.file.ends_with("xflow.js"));
        assert_eq!(f.writes, ["reports/**"]);
        assert_eq!(f.uses, ["codix"]);
        assert_eq!(f.input_schema, Some(serde_json::json!({"type": "object"})));
        assert_eq!(
            f.bytecode_dir.as_deref(),
            Some(
                dir.path()
                    .join(base::paths::entry::CACHE)
                    .join(".bytecode")
                    .as_path()
            )
        );
    }

    #[test]
    fn a_function_without_a_schema_takes_any_input() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("x.js"), "function run() {}").unwrap();
        let functions = load(
            dir.path(),
            "\n[[function]]\nname = \"run\"\nentry = \"x.js:run\"\ndescription = \"d\"\n",
        )
        .unwrap();
        assert_eq!(functions[0].input_schema, None);
        assert!(functions[0].writes.is_empty());
        assert_eq!(functions[0].bytecode_dir, None);
    }

    #[test]
    fn an_entry_that_is_bytecode_or_leaves_the_package_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("check.json"), "{}").unwrap();
        std::fs::write(dir.path().join("xflow.js"), b"\x05\x00\x02check\x00\xff").unwrap();
        let e = load(dir.path(), CHECK).unwrap_err().to_string();
        assert!(
            e.contains("function `check`") && e.contains("never compiled QuickJS bytecode"),
            "{e}"
        );
        std::fs::write(
            dir.path().join("xflow.js"),
            "x".repeat(crate::commands::MAX_ENTRY_BYTES as usize + 1),
        )
        .unwrap();
        let e = load(dir.path(), CHECK).unwrap_err().to_string();
        assert!(e.contains("over the"), "{e}");
        #[cfg(unix)]
        {
            std::fs::remove_file(dir.path().join("xflow.js")).unwrap();
            let outside = tempfile::tempdir().unwrap();
            std::fs::write(outside.path().join("x.js"), "function check() {}").unwrap();
            std::os::unix::fs::symlink(outside.path().join("x.js"), dir.path().join("xflow.js"))
                .unwrap();
            let e = load(dir.path(), CHECK).unwrap_err().to_string();
            assert!(e.contains("outside the package"), "{e}");
        }
    }

    #[test]
    fn a_schema_that_is_missing_not_json_or_not_a_schema_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("xflow.js"), "function check() {}").unwrap();
        assert!(load(dir.path(), CHECK).is_err(), "no check.json");
        for (body, says) in [
            ("{not json", "is not JSON"),
            (r#"{"type": 12}"#, "is not a JSON Schema"),
        ] {
            std::fs::write(dir.path().join("check.json"), body).unwrap();
            let e = load(dir.path(), CHECK).unwrap_err().to_string();
            assert!(
                e.contains(says) && e.contains("input_schema `check.json`"),
                "{e}"
            );
        }
        std::fs::write(
            dir.path().join("check.json"),
            format!("\"{}\"", "x".repeat(MAX_SCHEMA_BYTES as usize)),
        )
        .unwrap();
        let e = load(dir.path(), CHECK).unwrap_err().to_string();
        assert!(e.contains("over the"), "{e}");
    }
}
