//! Agix 的嵌入门面：嵌入方只依赖这一个 crate，引擎的嵌入面按主题在这里再导出。
//! 本 crate 自身不含任何逻辑，只有 `pub use` 与 `pub mod`；用法见 `docs/agix-sdk.md`。

pub mod contracts;
pub mod embed;
pub mod events;
pub mod history;
pub mod host_tools;
pub mod modules;
pub mod pool;
pub mod prelude;
pub mod process;
pub mod prompts;
pub mod recordings;
pub mod rpc;
pub mod scenes;
pub mod session;
pub mod settings;

#[doc = include_str!("../../../docs/agix-sdk.md")]
pub mod guide {}
