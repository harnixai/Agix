//! 宿主自己执行、以会话工具的身份给模型的工具：在 `SessionOptions::host_tools` 声明，用 `SessionPool::respond_tool_call` 回答；
//! 也可以由一个订阅者为之后建起的每个会话提供（`SessionPool::set_host_tools`，用 `respond_tool_call_from` 回答），
//! 或注册连接级 hook（`SessionPool::set_hooks`，用 `respond_hook` 回答）。

pub use ::assembly::session_pool::api::{
    HookAnswer, HooksSet, HostHookOnError, HostHookOption, HostHookSet, HostToolOption,
    HostToolPermit, HostToolsSet, RefusedEntry,
};
pub use ::base::interface::host_call::{HostId, HostReply};
