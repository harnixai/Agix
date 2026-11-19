//! The checks a manifest has to pass before anything reads its files: each
//! path stays inside the package, each name and entry is well formed, each
//! `writes` glob stays inside the project as written.

use super::{
    check_engine, inside_package, AgentDef, CommandDef, FunctionDef, McpPayload, PluginError,
    PluginManifest, ScriptPayload, SUPPORTED_API_VERSIONS,
};
use std::path::Path;

pub(super) fn validate(m: &PluginManifest) -> Result<(), PluginError> {
    if m.plugin.name.trim().is_empty() {
        return Err(PluginError::Schema("plugin.name must not be empty".into()));
    }
    if !SUPPORTED_API_VERSIONS.contains(&m.plugin.api_version.as_str()) {
        return Err(PluginError::ApiVersion {
            found: m.plugin.api_version.clone(),
            supported: SUPPORTED_API_VERSIONS.join(", "),
        });
    }
    if let Some(required) = &m.plugin.requires_engine {
        if required.comparators.iter().any(|c| !c.pre.is_empty()) {
            return Err(PluginError::Schema(format!(
                "requires_engine `{required}` names a pre-release; the engine is compared by \
                 its x.y.z alone, so write the requirement without a suffix"
            )));
        }
        check_engine(required, env!("CARGO_PKG_VERSION"))?;
    }
    validate_scripts(&m.script)?;

    if let Some(dir) = &m.scene.dir {
        if !inside_package(dir) {
            return Err(PluginError::Schema(format!(
                "`[scene] dir` `{}` must name a directory inside the package",
                dir.display()
            )));
        }
    }

    if let Some(skills) = &m.skills {
        if !inside_package(&skills.dir) {
            return Err(PluginError::Schema(format!(
                "`[skills] dir` `{}` must name a directory inside the package",
                skills.dir.display()
            )));
        }
    }

    validate_agents(&m.agent)?;
    validate_commands(&m.command)?;
    validate_functions(&m.function)?;
    validate_mcp(&m.mcp)
}

fn validate_scripts(scripts: &[ScriptPayload]) -> Result<(), PluginError> {
    for s in scripts {
        if s.point.trim().is_empty() {
            return Err(PluginError::Schema(
                "a `[[script]]` declares no `point`".into(),
            ));
        }
        let Some((file, function)) = s.entry.rsplit_once(':') else {
            return Err(PluginError::Schema(format!(
                "script entry `{}` is not `<file>:<function>`",
                s.entry
            )));
        };
        if file.is_empty() || function.is_empty() {
            return Err(PluginError::Schema(format!(
                "script entry `{}` is not `<file>:<function>`",
                s.entry
            )));
        }
        // The path is resolved against the package's own directory, so
        // anything that could leave it is refused here rather than relied on
        // being caught later — the later check is in a different crate, and a
        // package that reaches outside itself is the one case where "it
        // probably fails anyway" is not good enough.
        if !inside_package(Path::new(file)) {
            return Err(PluginError::Schema(format!(
                "script entry `{}` must name a file inside the package",
                s.entry
            )));
        }
    }
    Ok(())
}

fn validate_mcp(servers: &[McpPayload]) -> Result<(), PluginError> {
    for s in servers {
        let Some(config) = &s.config else {
            return Err(PluginError::Schema(format!(
                "mcp `{}` declares no `config`",
                s.name
            )));
        };
        if !inside_package(config) {
            return Err(PluginError::Schema(format!(
                "mcp `{}` config `{}` must name a file inside the package",
                s.name,
                config.display()
            )));
        }
    }
    Ok(())
}

fn validate_agents(agents: &[AgentDef]) -> Result<(), PluginError> {
    for a in agents {
        if !inside_package(&a.prompt) {
            return Err(PluginError::Schema(format!(
                "agent `{}` prompt `{}` must name a file inside the package",
                a.name,
                a.prompt.display()
            )));
        }
    }
    Ok(())
}

fn validate_commands(commands: &[CommandDef]) -> Result<(), PluginError> {
    let mut names = std::collections::HashSet::new();
    for c in commands {
        validate_command(c)?;
        if !names.insert(c.name.as_str()) {
            return Err(PluginError::Schema(format!(
                "command `{}` is declared twice",
                c.name
            )));
        }
    }
    Ok(())
}

fn validate_command(c: &CommandDef) -> Result<(), PluginError> {
    let schema = |what: String| PluginError::Schema(format!("command `{}` {what}", c.name));
    validate_name(&c.name).map_err(|why| schema(why.into()))?;
    if base::command::BUILTIN_COMMANDS
        .iter()
        .any(|(builtin, _)| *builtin == c.name)
    {
        return Err(schema("takes the name of a built-in slash command".into()));
    }
    validate_entry(&c.entry, c.file(), c.function()).map_err(schema)?;
    if c.description.trim().is_empty() || c.description.contains(['\n', '\r']) {
        return Err(schema("needs a one-line description".into()));
    }
    if c.read_only
        .iter()
        .any(|p| !p.is_empty() && p.split_whitespace().next().is_none())
    {
        return Err(schema(
            "has a blank `read_only` prefix; `\"\"` is the call with no arguments".into(),
        ));
    }
    validate_writes(&c.writes).map_err(schema)
}

fn validate_functions(functions: &[FunctionDef]) -> Result<(), PluginError> {
    let mut names = std::collections::HashSet::new();
    for f in functions {
        validate_function(f)?;
        if !names.insert(f.name.as_str()) {
            return Err(PluginError::Schema(format!(
                "function `{}` is declared twice",
                f.name
            )));
        }
    }
    Ok(())
}

fn validate_function(f: &FunctionDef) -> Result<(), PluginError> {
    let schema = |what: String| PluginError::Schema(format!("function `{}` {what}", f.name));
    validate_name(&f.name).map_err(|why| schema(why.into()))?;
    validate_entry(&f.entry, f.file(), f.function()).map_err(schema)?;
    let limit = crate::disclosure::MAX_DESCRIPTION_CHARS;
    if f.description.trim().is_empty() || f.description.contains(['\n', '\r']) {
        return Err(schema("needs a one-line description".into()));
    }
    if f.description.chars().count() > limit {
        return Err(schema(format!(
            "has a description over the {limit} characters allowed"
        )));
    }
    if let Some(rel) = &f.input_schema {
        if !inside_package(rel) {
            return Err(schema(format!(
                "input_schema `{}` must name a file inside the package",
                rel.display()
            )));
        }
    }
    validate_writes(&f.writes).map_err(schema)
}

fn validate_name(name: &str) -> Result<(), &'static str> {
    let fits = name.len() <= 32
        && name.starts_with(|ch: char| ch.is_ascii_lowercase())
        && name
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-');
    if fits {
        Ok(())
    } else {
        Err("must be 1–32 lowercase ASCII letters, digits or `-`, starting with a letter")
    }
}

fn validate_entry(entry: &str, file: &str, function: &str) -> Result<(), String> {
    if file.is_empty() || !is_identifier(function) {
        return Err(format!(
            "entry `{entry}` is not `<file>:<function>` with a JavaScript identifier"
        ));
    }
    if !inside_package(Path::new(file)) {
        return Err(format!(
            "entry `{entry}` must name a file inside the package"
        ));
    }
    Ok(())
}

fn validate_writes(writes: &[String]) -> Result<(), String> {
    for glob in writes {
        if let Some(why) = unscoped_write(glob) {
            return Err(format!(
                "`writes` `{glob}` {why}; write a glob relative to the project, below a \
                 directory in it (`data/**`)"
            ));
        }
    }
    Ok(())
}

/// Why a `writes` glob would reach beyond the project as written. Where it
/// lands once resolved against a project is checked again where it is used.
fn unscoped_write(glob: &str) -> Option<&'static str> {
    let glob = glob.trim();
    if glob.is_empty() {
        return Some("is empty");
    }
    if Path::new(glob).is_absolute() || glob.starts_with('~') || glob.starts_with('\\') {
        return Some("is not relative to the project");
    }
    if glob.split(['/', '\\']).any(|part| part == "..") {
        return Some("climbs out with `..`");
    }
    let literal = glob
        .split('/')
        .take_while(|part| !part.contains(['*', '?', '[', '{']))
        .filter(|part| !part.is_empty() && *part != ".")
        .count();
    (literal == 0).then_some("covers the whole project")
}

fn is_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_' || c == '$')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
}
