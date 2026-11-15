//! 建会话、跑一轮：`SessionPool` 的方法收的请求与回的结局。

pub use ::assembly::rpc::methods::turn::CompactAtParam;
pub use ::assembly::rpc::methods::turn::CompactParams;
pub use ::assembly::session_pool::api::Initiator;
pub use ::assembly::session_pool::api::SessionOptions;
pub use ::assembly::session_pool::api::TurnOutcome;
pub use ::assembly::session_pool::api::TurnRequest;
pub use ::assembly::session_pool::OnBusy;
pub use ::assembly::session_pool::ProjectSelector;
pub use ::base::session::SessionId;
