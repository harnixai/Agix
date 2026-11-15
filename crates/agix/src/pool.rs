//! 把设置、场景和模型客户端装成一个会话池；所有嵌入都从这里开始。

pub use ::assembly::assemble::pool as assemble;
pub use ::assembly::assemble::Assembly;
pub use ::assembly::assemble::Transcripts;
pub use ::assembly::config::load_daemon_config;
pub use ::assembly::config::DaemonConfig;
pub use ::assembly::config::DefaultDaemonPaths;
pub use ::assembly::config::StaticDaemonPaths;
pub use ::assembly::session_pool::api::InvalidOption;
pub use ::assembly::session_pool::api::PoolError;
pub use ::assembly::SessionCtx;
pub use ::assembly::SessionCustomizer;
pub use ::assembly::SessionPool;
