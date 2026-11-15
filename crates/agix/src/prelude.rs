//! 跑起一个池、建一个会话、跑一轮所必需的最小集合；`use agix::prelude::*` 之后，指南 §3 的片段不再需要别的 `use`。

pub use crate::pool::{assemble, load_daemon_config, Assembly, SessionPool, StaticDaemonPaths};
pub use crate::scenes::builtin;
pub use crate::session::{Initiator, OnBusy, ProjectSelector, TurnOutcome, TurnRequest};
