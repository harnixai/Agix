//! 嵌入入口：`agixd` 的参数进、协议连接出（指南 §10），连同 `EmbedConfig.js_libs` 用的宿主 JS 库类型。0.x 内单独承诺稳定。

pub use ::assembly::embed::EmbedConfig;
pub use ::assembly::embed::EmbedError;
pub use ::assembly::embed::Embedded;
pub use ::base::interface::host_js::HostCallContext;
pub use ::base::interface::host_js::HostJsError;
pub use ::base::interface::host_js::HostJsLib;
