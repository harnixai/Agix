//! 斜杠命令目录与定时任务的领域类型。

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CommandInfo {
    pub name: String,
    pub description: String,
    pub kind: CommandKind,
    pub source: CommandSource,
    /// Only for a `user` skill read from a user-level directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier: Option<CommandTier>,
    /// Absolute path of the directory a file-based skill lives in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dir: Option<String>,
}

/// Which user-level skills directory a `user` command came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CommandTier {
    /// `<state root>/skills/`, shared by every scene.
    Global,
    /// `<state root>/scenes/<scene>/skills/`.
    Scene,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[non_exhaustive]
pub enum CommandKind {
    /// Runs immediately, no model turn.
    #[serde(rename = "local")]
    Local,
    /// Expands a skill body and goes on to the model.
    #[serde(rename = "prompt")]
    Prompt,
    #[serde(rename = "mcp-prompt")]
    McpPrompt,
    /// A plugin command, run through `RunExt`; no model turn.
    #[serde(rename = "ext")]
    Ext,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CommandSource {
    Builtin,
    User,
    Project,
    Plugin,
    /// A directory the host listed for the session (`options.skill_dirs`).
    Host,
    Mcp,
}

impl From<runtime::commands::CommandInfo> for CommandInfo {
    fn from(info: runtime::commands::CommandInfo) -> Self {
        let kind = match info.kind {
            "local" => CommandKind::Local,
            "prompt" => CommandKind::Prompt,
            "ext" => CommandKind::Ext,
            _ => CommandKind::McpPrompt,
        };
        let source = match info.source {
            "builtin" => CommandSource::Builtin,
            "user" => CommandSource::User,
            "project" => CommandSource::Project,
            "plugin" => CommandSource::Plugin,
            "host" => CommandSource::Host,
            _ => CommandSource::Mcp,
        };
        Self {
            name: info.name,
            description: info.description,
            kind,
            source,
            tier: match info.tier {
                Some("global") => Some(CommandTier::Global),
                Some("scene") => Some(CommandTier::Scene),
                _ => None,
            },
            dir: info.dir.map(|d| d.display().to_string()),
        }
    }
}

/// A job to schedule: `prompt` runs as a turn of `session_id` whenever
/// `cron` matches.
#[derive(Debug, Clone)]
pub struct NewSchedule {
    pub session_id: String,
    pub cron: String,
    pub prompt: String,
    pub recurring: bool,
    pub durable: bool,
}
