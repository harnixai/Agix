//! 插件目录与安装结果的领域类型。不依赖 plugin crate，没编进插件包方案的构建里
//! 也能用。

use serde::{Deserialize, Serialize};

/// Which tier's plugin directory a request acts on.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum PluginScope {
    /// Under the global root, shared by every scene.
    #[default]
    Global,
    /// Under the scene root, for this daemon's scene only.
    Scene,
}

impl PluginScope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Scene => "scene",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum SkillOrigin {
    User,
    Project,
    Plugin,
    Host,
}

/// A package skill whose name another source holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SkillConflict {
    pub skill: String,
    pub shadowed_by: SkillOrigin,
    /// The `SKILL.md` that holds the name.
    pub path: String,
    /// The other package, when `shadowed_by` is `plugin`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin: Option<String>,
}

/// A command a package adds (`[[command]]`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PluginCommand {
    pub name: String,
    pub description: String,
    /// Argument prefixes that make a call read-only.
    pub read_only: Vec<String>,
    /// Globs relative to the project that the command may write.
    pub writes: Vec<String>,
}

/// A function a package exposes to the host (`[[function]]`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PluginFunctionEntry {
    /// Called as `<plugin>.<name>`.
    pub name: String,
    pub description: String,
    /// Globs relative to the project that a writing call may write.
    pub writes: Vec<String>,
    /// Whether the input is checked against a schema the package ships.
    pub has_input_schema: bool,
}

/// Where a package comes from.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum PluginOrigin {
    /// `plugin.install`, into a tier's cache.
    #[default]
    Installed,
    /// The embedding host's own, from a directory it named; `plugin.uninstall`
    /// and `plugin.disable` do not reach it.
    Host,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CommandShadow {
    /// An earlier package (by name) declares the command: this one is not
    /// registered.
    Plugin,
    /// A skill has the name and takes the `/name` form; `RunExt` still runs
    /// the command.
    Skill,
}

/// A package command whose name another holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CommandConflict {
    pub command: String,
    pub shadowed_by: CommandShadow,
    /// The package that has the command, when `shadowed_by` is `plugin`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin: Option<String>,
    /// The skill's `SKILL.md`, when `shadowed_by` is `skill`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PluginInfo {
    pub name: String,
    pub version: String,
    pub description: String,
    pub source: PluginOrigin,
    pub enabled: bool,
    /// Why a package is not loaded regardless of its enable state:
    /// `settings.plugins.allow` does not name it, it uses a library the host
    /// does not provide, or the host has its own package of the name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled_reason: Option<String>,
    /// The host's own package of this name, which is loaded in its place.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shadowed_by: Option<PluginOrigin>,
    /// The unpacked directory.
    pub root: String,
    /// `[[script]]` bindings that could not be honoured, each with its reason.
    pub script_faults: Vec<String>,
    pub skills: Vec<String>,
    pub skill_conflicts: Vec<SkillConflict>,
    pub commands: Vec<PluginCommand>,
    pub command_conflicts: Vec<CommandConflict>,
    pub functions: Vec<PluginFunctionEntry>,
}

/// A piece of text that will reach the model, and where it came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct VisibleText {
    pub origin: String,
    pub text: String,
}

/// What a plugin will put into the model's context and start, shown before
/// anyone depends on it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PluginDisclosure {
    pub plugin: String,
    pub version: String,
    pub capabilities: Vec<String>,
    pub scene: Option<String>,
    pub skills: Vec<String>,
    pub commands: Vec<String>,
    /// What the host calls its functions by (`<plugin>.<name>`).
    pub functions: Vec<String>,
    pub mcp_servers: Vec<String>,
    pub model_visible: Vec<VisibleText>,
    pub inert: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PluginInstalled {
    pub success: bool,
    pub message: String,
    /// Whether it is enabled once installed (`plugins.enable_on_install`).
    pub enabled: bool,
    pub disclosure: PluginDisclosure,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PluginRemoved {
    pub success: bool,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PluginToggled {
    pub name: String,
    pub enabled: bool,
    pub scope: PluginScope,
}
