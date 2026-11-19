//! What a package needs from the host that embeds the engine — the libraries
//! its `uses` names — and the host's own packages, loaded from a directory
//! the host names rather than installed into a cache.

use crate::disclosure::Disclosure;
use crate::manifest::{Plugin, PluginError};
use base::command::HostLibrary;
use std::path::{Path, PathBuf};

/// The libraries `plugin` uses that `provided` does not have.
pub fn missing_libraries(plugin: &Plugin, provided: &[HostLibrary]) -> Vec<String> {
    plugin
        .manifest
        .uses
        .iter()
        .filter(|name| !provided.iter().any(|l| &l.name == *name))
        .cloned()
        .collect()
}

pub fn check_libraries(plugin: &Plugin, provided: &[HostLibrary]) -> Result<(), PluginError> {
    let missing = missing_libraries(plugin, provided);
    if missing.is_empty() {
        Ok(())
    } else {
        Err(PluginError::MissingLibrary(missing))
    }
}

/// Load the package unpacked at `dir` as one of the host's own: checked like
/// an install, its compiled entries kept in `bytecode_dir`.
pub fn load_host_package(
    dir: &Path,
    layout: &base::paths::Layout,
    provided: &[HostLibrary],
    bytecode_dir: PathBuf,
) -> Result<(Plugin, Disclosure), PluginError> {
    let root = dir.canonicalize().map_err(|e| {
        PluginError::Io(std::io::Error::new(
            e.kind(),
            format!("{}: {e}", dir.display()),
        ))
    })?;
    let plugin = Plugin::load(&root, &root.join("plugin.toml"), layout)?
        .with_bytecode_dir(bytecode_dir)
        .from_host();
    check_libraries(&plugin, provided)?;
    let disclosure = Disclosure::from_plugin(&plugin, provided)?;
    Ok((plugin, disclosure))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn codix() -> HostLibrary {
        HostLibrary {
            name: "codix".into(),
            description: Some("the host's code index".into()),
        }
    }

    fn package(dir: &Path, uses: &str) {
        std::fs::write(
            dir.join("plugin.toml"),
            format!(
                "{uses}\n[plugin]\nname = \"xflow\"\nversion = \"1.0.0\"\napi_version = \"0.1\"\n\
                 [[function]]\nname = \"check\"\nentry = \"x.js:check\"\ndescription = \"d\"\n"
            ),
        )
        .unwrap();
        std::fs::write(dir.join("x.js"), "function check() {}").unwrap();
    }

    #[test]
    fn a_host_package_loads_from_its_directory_with_the_bytecode_dir_it_is_given() {
        let dir = tempfile::tempdir().unwrap();
        package(dir.path(), "uses = [\"codix\"]");
        let keep = dir.path().join("kept");
        let (plugin, disclosure) =
            load_host_package(dir.path(), &Default::default(), &[codix()], keep.clone()).unwrap();
        assert_eq!(plugin.bytecode_dir(), Some(keep.as_path()));
        assert_eq!(disclosure.functions, ["xflow.check"]);
        let functions = crate::functions::declared_functions(&plugin).unwrap();
        assert_eq!(functions[0].bytecode_dir, Some(keep));
    }

    #[test]
    fn a_library_the_host_does_not_provide_is_named_in_the_refusal() {
        let dir = tempfile::tempdir().unwrap();
        package(dir.path(), "uses = [\"codix\", \"tracker\", \"codix\"]");
        let e = load_host_package(dir.path(), &Default::default(), &[], dir.path().join("k"))
            .unwrap_err()
            .to_string();
        assert_eq!(
            e,
            "needs host libraries `codix`, `tracker`, which this host does not provide"
        );
        let e = load_host_package(
            dir.path(),
            &Default::default(),
            &[codix()],
            dir.path().join("k"),
        )
        .unwrap_err()
        .to_string();
        assert_eq!(
            e,
            "needs host library `tracker`, which this host does not provide"
        );
    }

    #[test]
    fn a_missing_directory_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_host_package(
            &dir.path().join("absent"),
            &Default::default(),
            &[],
            dir.path().join("k")
        )
        .is_err());
    }
}
