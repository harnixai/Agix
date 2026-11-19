//! Slash-command names the engine answers itself, the plugin commands
//! (`[[command]]`) a session runs through `RunExt` and as `/name args`, the
//! plugin functions (`[[function]]`) only the host calls, and the host
//! libraries (`uses`) a package's code may need.

use std::path::PathBuf;

/// The engine's own `/name` commands, with what each does. A plugin command
/// may not take one of these names.
pub const BUILTIN_COMMANDS: &[(&str, &str)] = &[
    ("help", "List all available slash commands"),
    ("skills", "List all available skills"),
    ("clear", "Clear the current session context"),
    ("compact", "Trigger context compaction now"),
    ("cost", "Show the tokens and calls this session has spent"),
];

/// One command an enabled package declares, as the session runs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtCommand {
    pub name: String,
    pub description: String,
    pub plugin: String,
    pub plugin_version: String,
    /// The package's unpacked directory.
    pub root: PathBuf,
    /// The entry file, inside `root`.
    pub file: PathBuf,
    /// The function the entry file defines that the command calls.
    pub function: String,
    /// Argument prefixes, whole words each, that make a call read-only; `""`
    /// is the call with no arguments.
    pub read_only: Vec<String>,
    /// Globs relative to the project root that the command may write.
    pub writes: Vec<String>,
    /// The host libraries the package declares, by namespace.
    pub uses: Vec<String>,
    /// Where compiled entries are kept between runs; `None` compiles every run.
    pub bytecode_dir: Option<PathBuf>,
}

/// One function an enabled package declares for the host to call, never the
/// model: `<plugin>.<name>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginFunction {
    pub name: String,
    pub plugin: String,
    pub plugin_version: String,
    pub description: String,
    /// The package's unpacked directory.
    pub root: PathBuf,
    /// The entry file, inside `root`.
    pub file: PathBuf,
    /// The function the entry file defines, called as `(input, ctx)`.
    pub function: String,
    /// Globs relative to the project root that a writing call may write.
    pub writes: Vec<String>,
    /// The JSON Schema `input` is checked against before the call.
    pub input_schema: Option<serde_json::Value>,
    /// The host libraries the package declares, by namespace.
    pub uses: Vec<String>,
    /// Where compiled entries are kept between runs; `None` compiles every run.
    pub bytecode_dir: Option<PathBuf>,
}

impl PluginFunction {
    /// The name a host calls it by.
    pub fn call_name(&self) -> String {
        format!("{}.{}", self.plugin, self.name)
    }
}

/// A JavaScript library the host provides to plugin code, as a package's
/// `uses` names it and its install disclosure describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostLibrary {
    pub name: String,
    pub description: Option<String>,
}

impl ExtCommand {
    /// Whether a call with `args` is read-only: they start with one of the
    /// `read_only` prefixes, word for word, or there are none and `""` is
    /// one of them.
    pub fn is_read_only(&self, args: &[String]) -> bool {
        self.read_only.iter().any(|prefix| {
            let words: Vec<&str> = prefix.split_whitespace().collect();
            if words.is_empty() {
                return args.is_empty();
            }
            words.len() <= args.len() && words.iter().zip(args).all(|(w, a)| *w == a)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command(read_only: &[&str]) -> ExtCommand {
        ExtCommand {
            name: "forge".into(),
            description: String::new(),
            plugin: "forge".into(),
            plugin_version: "1.0.0".into(),
            root: PathBuf::new(),
            file: PathBuf::new(),
            function: "main".into(),
            read_only: read_only.iter().map(|s| s.to_string()).collect(),
            writes: Vec::new(),
            uses: Vec::new(),
            bytecode_dir: None,
        }
    }

    fn args(line: &str) -> Vec<String> {
        line.split_whitespace().map(str::to_string).collect()
    }

    #[test]
    fn a_read_only_prefix_matches_whole_words_only() {
        let c = command(&["help", "spec list"]);
        assert!(c.is_read_only(&args("help")));
        assert!(c.is_read_only(&args("spec list --status draft")));
        assert!(!c.is_read_only(&args("spec listall")));
        assert!(!c.is_read_only(&args("spec")));
        assert!(!c.is_read_only(&args("helpme")));
        assert!(!c.is_read_only(&[]));
    }

    #[test]
    fn an_empty_prefix_is_the_call_with_no_arguments_only() {
        let bare = command(&[""]);
        assert!(bare.is_read_only(&[]));
        assert!(!bare.is_read_only(&args("anything")));
        assert!(!bare.is_read_only(&args("help")));
    }
}
