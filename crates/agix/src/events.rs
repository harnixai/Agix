//! 会话里发生的事怎么到宿主：实现 `SessionWatcher`，池把每一帧同步交给它。

pub use ::assembly::session_pool::api::SessionWatcher;
pub use ::assembly::session_pool::api::WatcherId;
pub use ::assembly::session_pool::api::SessionFrame;
pub use ::assembly::session_pool::api::SessionEvent;
pub use ::assembly::session_pool::api::Subscribed;
pub use ::assembly::session_pool::api::AskKind;
pub use ::base::event::AgentEvent;
