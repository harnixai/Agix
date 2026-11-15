//! 会话的配置与状态，按读写性质拆开，而不是一个什么都装的上下文对象：
//! - **只读配置** → `Arc<EngineConfig>`
//! - **可变运行期** → `Arc<dyn Session>`（内部按需 split lock）

pub mod config;
pub mod goal;
pub mod ledger;
pub mod session;
pub mod task;

pub use self::config::*;
pub use self::ledger::*;
pub use self::session::*;
pub use self::task::*;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deepseek_infers_1m_window() {
        assert_eq!(infer_context_window_tokens("deepseek-v4-flash"), 1_000_000);
        assert_eq!(infer_context_window_tokens("ds-v4-pro"), 1_000_000);
    }

    #[test]
    fn codex_infers_400k_window() {
        assert_eq!(infer_context_window_tokens("gpt-5.2-codex"), 400_000);
    }
}
