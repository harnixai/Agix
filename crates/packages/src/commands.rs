//! The commands a package adds (`[[command]]`), as `RunExt` runs them, and
//! the one check on their files — and on `[[function]]` entries — no manifest
//! field can carry: an entry is JavaScript source, never bytecode.
//!
//! QuickJS does not verify bytecode, so a crafted file can corrupt the host's
//! memory; it is also tied to one QuickJS build and cannot be read at
//! install. The engine compiles each entry itself and keeps the result in
//! the cache's `.bytecode` directory (`ptc`).

use crate::manifest::{Plugin, PluginError};
use base::command::ExtCommand;
use std::path::{Path, PathBuf};

/// The most an entry file may hold.
pub const MAX_ENTRY_BYTES: u64 = 4 * 1024 * 1024;

/// Every command `plugin` declares, each entry checked to be source. Read at
/// install and again at discovery, so a package whose entry changed since
/// is refused then.
pub fn declared_commands(plugin: &Plugin) -> Result<Vec<ExtCommand>, PluginError> {
    plugin
        .manifest
        .command
        .iter()
        .map(|c| {
            let file = entry_file(plugin, &format!("command `{}`", c.name), c.file())?;
            Ok(ExtCommand {
                name: c.name.clone(),
                description: c.description.clone(),
                plugin: plugin.name().to_string(),
                plugin_version: plugin.manifest.plugin.version.clone(),
                root: plugin.root.clone(),
                file,
                function: c.function().to_string(),
                read_only: c.read_only.clone(),
                writes: c.writes.clone(),
                uses: plugin.manifest.uses.clone(),
                bytecode_dir: plugin.bytecode_dir().map(Path::to_path_buf),
            })
        })
        .collect()
}

/// `entry`, resolved inside `plugin` and checked to be source of a size the
/// engine runs; `what` names the declaration in a refusal.
pub(crate) fn entry_file(plugin: &Plugin, what: &str, entry: &str) -> Result<PathBuf, PluginError> {
    let refused = |why: String| PluginError::Schema(format!("{what} {why}"));
    let file = plugin
        .path(Path::new(entry))
        .map_err(|e| refused(format!("entry: {e}")))?;
    let size = std::fs::metadata(&file)?.len();
    if size > MAX_ENTRY_BYTES {
        return Err(refused(format!(
            "entry `{entry}` is {size} bytes, over the {MAX_ENTRY_BYTES} allowed"
        )));
    }
    let bytes = std::fs::read(&file)?;
    if bytes.contains(&0) || std::str::from_utf8(&bytes).is_err() {
        return Err(refused(format!(
            "entry `{entry}` is not JavaScript source: a package brings source only, never \
             compiled QuickJS bytecode; the engine compiles it on this machine"
        )));
    }
    Ok(file)
}

/// Where the compiled entries of a package unpacked at `root` — a version
/// directory of a cache (`.../cache/<name>/<version>`) — are kept.
pub fn bytecode_dir(root: &Path) -> Option<PathBuf> {
    let cache = root.parent()?.parent()?;
    (cache.file_name()? == base::paths::entry::CACHE).then(|| cache.join(".bytecode"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEAD: &str = "[plugin]\nname = \"demo\"\nversion = \"1.0.0\"\napi_version = \"0.1\"\n\
        [[command]]\nname = \"demo\"\nentry = \"cmd.js:main\"\ndescription = \"does demo things\"\n\
        read_only = [\"list\"]\nwrites = [\"data/**\"]\n";

    fn load(root: &Path, entry: &[u8]) -> Result<Vec<ExtCommand>, PluginError> {
        std::fs::write(root.join("plugin.toml"), HEAD).unwrap();
        std::fs::write(root.join("cmd.js"), entry).unwrap();
        let plugin = Plugin::load(root, &root.join("plugin.toml"), &Default::default())?;
        declared_commands(&plugin)
    }

    #[test]
    fn a_command_carries_its_package_and_its_entry() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir
            .path()
            .join(base::paths::entry::CACHE)
            .join("demo/1.0.0");
        std::fs::create_dir_all(&root).unwrap();
        let commands = load(&root, b"function main() { return 1; }").unwrap();
        let c = &commands[0];
        assert_eq!((c.name.as_str(), c.plugin.as_str()), ("demo", "demo"));
        assert_eq!(c.function, "main");
        assert!(c.file.ends_with("cmd.js"));
        assert_eq!(c.read_only, ["list"]);
        assert_eq!(c.writes, ["data/**"]);
        assert_eq!(
            c.bytecode_dir.as_deref(),
            Some(
                dir.path()
                    .join(base::paths::entry::CACHE)
                    .join(".bytecode")
                    .as_path()
            )
        );
    }

    #[test]
    fn bytecode_in_place_of_source_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let e = load(dir.path(), b"\x05\x00\x02main\x00\x10\xff")
            .unwrap_err()
            .to_string();
        assert!(e.contains("never compiled QuickJS bytecode"), "{e}");
        assert!(load(dir.path(), &[0xff, 0xfe, b'a']).is_err(), "not UTF-8");
    }

    #[test]
    fn an_entry_outside_the_package_or_missing_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("plugin.toml"), HEAD).unwrap();
        let plugin = Plugin::load(
            dir.path(),
            &dir.path().join("plugin.toml"),
            &Default::default(),
        )
        .unwrap();
        assert!(declared_commands(&plugin).is_err(), "no cmd.js");
        #[cfg(unix)]
        {
            let outside = tempfile::tempdir().unwrap();
            std::fs::write(outside.path().join("x.js"), "function main() {}").unwrap();
            std::os::unix::fs::symlink(outside.path().join("x.js"), dir.path().join("cmd.js"))
                .unwrap();
            let e = declared_commands(&plugin).unwrap_err().to_string();
            assert!(e.contains("outside the package"), "{e}");
        }
    }

    #[test]
    fn a_package_outside_a_cache_keeps_no_bytecode() {
        assert_eq!(bytecode_dir(Path::new("/somewhere/demo")), None);
    }
}
