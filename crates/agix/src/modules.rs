//! Rust 扩展进会话的载体：`Plugin`（经 `Assembly::plugins`，每个会话、每次重建都注册）、`Builder`（在 `SessionCustomizer` 里改引擎）、`Module`（自己用 `Builder` 装会话时）。

pub use ::runtime::plugin::Plugin;
pub use ::runtime::plugin::PluginSession;
pub use ::runtime::plugin::Registrar;
pub use ::runtime::agent::Builder;
pub use ::runtime::agent::HostContracts;
pub use ::runtime::module::Module;
pub use ::runtime::module::SessionContext;
pub use ::base::agent_type::AgentTypeDefinition;
