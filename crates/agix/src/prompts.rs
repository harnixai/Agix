//! 回答引擎向人提的问：权限、`AskUserQuestion`、MCP 询问，交给 `SessionPool::respond_to_prompt`。

pub use ::assembly::session_pool::PromptAnswer;
pub use ::runtime::agent::PermissionDecision;
pub use ::runtime::agent::PersistScope;
pub use ::runtime::agent::QuestionAnswer;
pub use ::base::interface::elicitation::McpElicitationAnswer;
