//! Agix 的嵌入门面：嵌入方只依赖这一个 crate，引擎的嵌入面按主题在这里再导出。
//! 本 crate 自身不含任何逻辑，只有 `pub use` 与 `pub mod`；用法见 `docs/agix-sdk.md`。

pub mod pool;
pub mod session;
pub mod events;
pub mod prompts;
pub mod host_tools;
pub mod modules;
pub mod contracts;
pub mod scenes;
pub mod settings;
pub mod history;
pub mod recordings;
pub mod rpc;
pub mod process;
pub mod prelude;
