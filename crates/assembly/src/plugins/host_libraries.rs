//! Whether the host's JavaScript libraries (`Assembly::js_libs`,
//! `EmbedConfig::js_libs`) can be installed, checked once at start: each
//! namespace is a new global beside the engine's own and no other library's,
//! and each function path is dot-separated identifiers, none twice and none
//! inside another.

use base::interface::host_js::HostJsLib;
use std::collections::BTreeSet;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("host library `{namespace}`: {reason}")]
pub(crate) struct InvalidHostLibrary {
    pub(crate) namespace: String,
    pub(crate) reason: String,
}

/// `engine` is the engine's name, the base library's global.
pub(crate) fn check(libs: &[HostJsLib], engine: &str) -> Result<(), InvalidHostLibrary> {
    let mut seen = BTreeSet::new();
    for lib in libs {
        let invalid = |reason: String| InvalidHostLibrary {
            namespace: lib.namespace().to_string(),
            reason,
        };
        let name = lib.namespace();
        base::interface::script::check_global_name(name).map_err(|why| invalid(why.into()))?;
        // A package's `uses` cannot name `agix` whatever the engine is called.
        if name == engine || name == base::paths::DEFAULT_ENGINE {
            return Err(invalid(
                "it is the engine's own name, the base library's global".into(),
            ));
        }
        if !seen.insert(name) {
            return Err(invalid("another host library has this namespace".into()));
        }
        check_paths(lib).map_err(invalid)?;
    }
    Ok(())
}

fn check_paths(lib: &HostJsLib) -> Result<(), String> {
    let mut paths = BTreeSet::new();
    for path in lib.paths() {
        if !path.split('.').all(is_identifier) {
            return Err(format!(
                "function `{path}` is not dot-separated JavaScript identifiers"
            ));
        }
        if !paths.insert(path) {
            return Err(format!("function `{path}` is declared twice"));
        }
    }
    for path in &paths {
        if let Some(inner) = paths.iter().find(|other| {
            other
                .strip_prefix(*path)
                .is_some_and(|r| r.starts_with('.'))
        }) {
            return Err(format!(
                "`{path}` is a function and also holds the function `{inner}`"
            ));
        }
    }
    Ok(())
}

fn is_identifier(word: &str) -> bool {
    let mut chars = word.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_' || c == '$')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
        && word != "__proto__"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lib(namespace: &str, paths: &[&str]) -> HostJsLib {
        paths.iter().fold(HostJsLib::new(namespace), |lib, path| {
            lib.function(path, |_, input| async move { Ok(input) })
        })
    }

    fn reason(libs: &[HostJsLib]) -> String {
        check(libs, "agix").unwrap_err().reason
    }

    #[test]
    fn a_namespace_is_a_new_global_that_is_not_the_engines() {
        assert!(check(&[lib("codix", &["flow.start"])], "agix").is_ok());
        for bad in [
            "agix", "Codix", "tools", "console", "class", "1x", "a-b", "",
        ] {
            assert!(check(&[lib(bad, &[])], "agix").is_err(), "{bad:?}");
        }
        assert!(reason(&[lib("agix", &[])]).contains("engine's own name"));
        assert!(check(&[lib("foo", &[])], "foo")
            .unwrap_err()
            .reason
            .contains("engine's own name"));
        assert!(check(&[lib("agix", &[])], "foo").is_err());
        assert!(reason(&[lib("codix", &[]), lib("codix", &[])]).contains("another host library"));
    }

    #[test]
    fn function_paths_are_dotted_identifiers_none_twice_none_inside_another() {
        assert!(check(
            &[lib("codix", &["flow.start", "flow.runs", "$x", "_a.b2"])],
            "agix"
        )
        .is_ok());
        for bad in [
            "",
            "flow.",
            ".flow",
            "flow..start",
            "flow-start",
            "1flow",
            "a.__proto__",
            "a b",
        ] {
            assert!(
                reason(&[lib("codix", &[bad])]).contains("not dot-separated"),
                "{bad:?}"
            );
        }
        assert!(reason(&[lib("codix", &["a.b", "a.b"])]).contains("declared twice"));
        assert!(reason(&[lib("codix", &["flow", "flow.start"])]).contains("also holds"));
        assert!(check(&[lib("codix", &["flow", "flows.start"])], "agix").is_ok());
    }
}
