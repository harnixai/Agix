//! The prompt block type and the names the kernel's blocks carry. Assembly
//! itself is `runtime::prompt_assembly`; this file is only the data it produces.

use serde::{Deserialize, Serialize};

/// Protocol-agnostic prompt block.
///
/// Each Model implementation translates these into its API-specific format.
/// `cache_strategy` carries Anthropic cache_control semantics (Ephemeral/Global)
/// but is ignored by non-Anthropic models.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PromptBlock {
    pub role: BlockRole,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_strategy: Option<CacheStrategy>,
    /// What this block is — see the [`names`] module.
    ///
    /// Annotation only: no adapter reads it, so it never reaches the wire and
    /// two blocks differing only here are byte-identical to the model. It
    /// exists because block boundaries alone do not say which block is the
    /// skills inventory and which is the MCP instructions, and the count
    /// shifts with configuration (rules, MCP and `prompt.append` are each
    /// optional), so position is not a reliable answer.
    ///
    /// This is how an extension addresses a block: "put mine after
    /// `skills.catalog`", "replace `memory.session`". That only works if the
    /// names are stable, which is why the kernel's are a published contract
    /// and not a debugging label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Who put this block here.
    ///
    /// Separate from [`name`](Self::name) because they answer different
    /// questions: the name says which block this is, the origin says whose it
    /// is. Rearranging the prompt is a privileged act, and how privileged
    /// depends on where the rearranging came from — a plugin someone
    /// downloaded is not a script that same person wrote. Nothing enforces
    /// that yet; recording it truthfully is the precondition for enforcing it
    /// at all.
    #[serde(default, skip_serializing_if = "BlockOrigin::is_kernel")]
    pub origin: BlockOrigin,
}

/// The kernel's block names. **A published contract**: an extension that
/// positions itself relative to one of these is relying on the name to keep
/// meaning what it means, so these change the way any public name changes.
///
/// A scene that names its own sections keeps those names, prefixed with
/// `scene.`; [`SCENE_SKELETON`] is what a scene that names nothing gets.
pub mod names {
    /// The scene's system prompt, when the scene does not name its sections.
    pub const SCENE_SKELETON: &str = "scene.skeleton";
    /// Prefix for a scene that does name its sections.
    pub const SCENE_PREFIX: &str = "scene.";
    /// The inventory of available skills. A `User` block — it opens the
    /// conversation rather than joining the system prompt.
    pub const SKILLS_CATALOG: &str = "skills.catalog";
    /// How to use the file-based memory system.
    pub const MEMORY_SESSION: &str = "memory.session";
    /// The discovery index of `.agix/rules/` files.
    pub const RULES: &str = "rules";
    /// Instructions contributed by connected MCP servers.
    pub const MCP_INSTRUCTIONS: &str = "mcp.instructions";
    /// `settings.prompt.append` given as one text.
    pub const CONFIG_PROMPT_APPEND: &str = "config.prompt_append";
    /// Prefix of the configuration's blocks, among them each block of a
    /// `settings.prompt.append` list a settings file gave.
    pub const CONFIG_PREFIX: &str = "config.";
    /// Prefix of each block of a `settings.prompt.append` list the host gave.
    pub const HOST_PREFIX: &str = "host.";
    /// `settings.prompt.override`, which replaces everything else.
    pub const CONFIG_PROMPT_OVERRIDE: &str = "config.prompt_override";
}

/// Where a prompt block came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum BlockOrigin {
    /// The engine itself.
    #[default]
    Kernel,
    /// Written by whoever configured this deployment.
    Config,
    /// Contributed by an installed plugin, named by its plugin id.
    Plugin(String),
    /// Contributed by a script in this project, named by its path.
    Script(String),
    /// An entry of a `settings.prompt.append` list the host gave, named as
    /// the host named it.
    Host(String),
    /// An entry of a `settings.prompt.append` list a settings file gave,
    /// named as the file named it: `{"config": "<name>"}`.
    #[serde(untagged)]
    ConfigEntry { config: String },
}

impl BlockOrigin {
    /// Used to keep the field out of serialized output in the common case.
    pub fn is_kernel(&self) -> bool {
        matches!(self, Self::Kernel)
    }

    /// Whether this block came from something the user wrote themselves, as
    /// opposed to something they installed. The distinction trust decisions
    /// will be made on.
    pub fn is_local(&self) -> bool {
        matches!(
            self,
            Self::Kernel
                | Self::Config
                | Self::ConfigEntry { .. }
                | Self::Script(_)
                | Self::Host(_)
        )
    }
}

/// Where a block is delivered. `User` blocks open the conversation, as one
/// `<system-reminder>` message ahead of everything else, instead of joining
/// the system prompt: endpoints that cache the prefix in the order system,
/// tools, messages (DeepSeek) would otherwise re-read the whole tool table
/// whenever such a block differs, as the skills inventory does between
/// projects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BlockRole {
    System,
    User,
    Assistant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheStrategy {
    Ephemeral,
    Global,
}

impl PromptBlock {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: BlockRole::System,
            content: content.into(),
            cache_strategy: None,
            name: None,
            origin: BlockOrigin::Kernel,
        }
    }

    pub fn system_cached(content: impl Into<String>) -> Self {
        Self {
            role: BlockRole::System,
            content: content.into(),
            cache_strategy: Some(CacheStrategy::Ephemeral),
            name: None,
            origin: BlockOrigin::Kernel,
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: BlockRole::User,
            content: content.into(),
            cache_strategy: None,
            name: None,
            origin: BlockOrigin::Kernel,
        }
    }

    pub fn named(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    pub fn from_origin(mut self, origin: BlockOrigin) -> Self {
        self.origin = origin;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The names are a contract an extension positions itself against, so a
    /// rename is a breaking change and has to look like one. This is the test
    /// that makes it look like one.
    #[test]
    fn the_kernel_block_names_are_what_they_are_published_as() {
        assert_eq!(names::SCENE_SKELETON, "scene.skeleton");
        assert_eq!(names::SKILLS_CATALOG, "skills.catalog");
        assert_eq!(names::MEMORY_SESSION, "memory.session");
        assert_eq!(names::RULES, "rules");
        assert_eq!(names::MCP_INSTRUCTIONS, "mcp.instructions");
        assert_eq!(names::CONFIG_PROMPT_APPEND, "config.prompt_append");
        assert_eq!(names::CONFIG_PROMPT_OVERRIDE, "config.prompt_override");
        assert_eq!(names::HOST_PREFIX, "host.");
        assert_eq!(names::CONFIG_PREFIX, "config.");
    }

    #[test]
    fn a_settings_file_entry_reads_and_writes_as_config_with_its_name() {
        let entry = BlockOrigin::ConfigEntry {
            config: "review".into(),
        };
        let json = serde_json::json!({"config": "review"});
        assert_eq!(serde_json::to_value(&entry).unwrap(), json);
        assert_eq!(serde_json::from_value::<BlockOrigin>(json).unwrap(), entry);
        assert_eq!(
            serde_json::from_value::<BlockOrigin>(serde_json::json!("config")).unwrap(),
            BlockOrigin::Config
        );
        assert_eq!(
            serde_json::from_value::<BlockOrigin>(serde_json::json!({"host": "a"})).unwrap(),
            BlockOrigin::Host("a".into())
        );
    }
}
