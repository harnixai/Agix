//! 宿主自己执行、以会话工具的身份给模型的工具：在 `SessionOptions::host_tools` 声明，用 `SessionPool::respond_tool_call` 回答。

pub use ::assembly::session_pool::api::HostToolOption;
pub use ::base::interface::host_call::HostReply;
