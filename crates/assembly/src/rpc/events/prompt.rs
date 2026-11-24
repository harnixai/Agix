//! The three questions a session puts to its host, as the `prompt` frame
//! carries them.

use base::event::AgentEvent;
use base::interface::elicitation::ElicitOption;
use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "prompt_type", rename_all = "snake_case")]
pub enum PromptPayload {
    Permission {
        prompt_id: String,
        tool_name: String,
        message: String,
        paths: Vec<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tool_use_id: Option<String>,
        /// The plugin command run whose sub-call this is.
        #[serde(skip_serializing_if = "Option::is_none")]
        origin: Option<base::interface::elicitation::PromptOrigin>,
        permit_always: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        task_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        agent_session_id: Option<String>,
    },
    Question {
        prompt_id: String,
        tool_use_id: Option<String>,
        question: String,
        header: Option<String>,
        options: Vec<ElicitOption>,
        #[serde(skip_serializing_if = "Option::is_none")]
        task_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        agent_session_id: Option<String>,
    },
    /// The server's words: `message`, `requested_schema`, `url`.
    McpElicitation {
        prompt_id: String,
        server: String,
        mode: String,
        message: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        requested_schema: Option<Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        url: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        url_host: Option<String>,
        #[serde(skip_serializing_if = "std::ops::Not::not")]
        limited: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        tool_use_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        task_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        agent_session_id: Option<String>,
    },
}

impl PromptPayload {
    pub(super) fn of(event: &AgentEvent) -> Option<Self> {
        Some(match event {
            AgentEvent::PermissionPrompt {
                prompt_id,
                tool_name,
                message,
                paths,
                turn_id: _,
                tool_use_id,
                task_id,
                agent_session_id,
                origin,
                permit_always,
            } => PromptPayload::Permission {
                prompt_id: prompt_id.clone(),
                tool_name: tool_name.clone(),
                message: message.clone(),
                paths: paths.iter().map(|p| p.display().to_string()).collect(),
                tool_use_id: tool_use_id.clone(),
                origin: origin.clone(),
                permit_always: *permit_always,
                task_id: task_id.clone(),
                agent_session_id: agent_session_id.clone(),
            },
            AgentEvent::QuestionPrompt {
                prompt_id,
                question,
                header,
                options,
                turn_id: _,
                tool_use_id,
                task_id,
                agent_session_id,
            } => PromptPayload::Question {
                prompt_id: prompt_id.clone(),
                tool_use_id: tool_use_id.clone(),
                question: question.clone(),
                header: header.clone(),
                options: options.clone(),
                task_id: task_id.clone(),
                agent_session_id: agent_session_id.clone(),
            },
            AgentEvent::McpElicitationPrompt {
                prompt_id,
                server,
                mode,
                message,
                requested_schema,
                url,
                url_host,
                limited,
                turn_id: _,
                tool_use_id,
                task_id,
                agent_session_id,
            } => PromptPayload::McpElicitation {
                prompt_id: prompt_id.clone(),
                server: server.clone(),
                mode: mode.clone(),
                message: message.clone(),
                requested_schema: requested_schema.clone(),
                url: url.clone(),
                url_host: url_host.clone(),
                limited: *limited,
                tool_use_id: tool_use_id.clone(),
                task_id: task_id.clone(),
                agent_session_id: agent_session_id.clone(),
            },
            _ => return None,
        })
    }
}
