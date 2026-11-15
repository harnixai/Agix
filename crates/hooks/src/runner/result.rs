//! 一次 `run` 的聚合结果 `HookRunResult`：各 hook 的结局，以及合并之后的拦下、批准、
//! 中止与改写。

use super::HookOutcome;
use crate::payload::{HookDecision, HookResponse};

/// 一次 `run` 的聚合结果。便于上层短路 / 统计。
#[derive(Debug, Clone, Default)]
pub struct HookRunResult {
    pub outcomes: Vec<HookOutcome>,
    /// What each outcome's hook is called, in step with `outcomes`.
    pub hooks: Vec<String>,
}

impl HookRunResult {
    /// 任意 hook 决议 block → 整体 block
    pub fn blocked(&self) -> Option<&HookResponse> {
        self.outcomes.iter().find_map(|o| match o {
            HookOutcome::Ran { response, .. } if response.decision == Some(HookDecision::Block) => {
                Some(response)
            }
            _ => None,
        })
    }
    /// 第一条 approve；若有 hook 改写了入参，只认给出生效改写的那条 hook 自己的 approve——
    /// 别的 hook 批准的是它看到的原入参，不能替改写后的入参答应询问
    pub fn approved(&self) -> Option<&HookResponse> {
        let rewrite_at = self.outcomes.iter().position(
            |o| matches!(o, HookOutcome::Ran { response, .. } if response.updated_input.is_some()),
        );
        self.outcomes.iter().enumerate().find_map(|(i, o)| match o {
            HookOutcome::Ran { response, .. }
                if response.decision == Some(HookDecision::Approve)
                    && rewrite_at.is_none_or(|at| at == i) =>
            {
                Some(response)
            }
            _ => None,
        })
    }
    /// 任意 hook 要求中止整个 turn
    pub fn discontinued(&self) -> bool {
        self.outcomes
            .iter()
            .any(|o| matches!(o, HookOutcome::Ran { response, .. } if response.r#continue == Some(false)))
    }
    /// 第一个要求中止的 hook 给出的 `message`
    pub fn discontinue_message(&self) -> Option<&str> {
        self.outcomes.iter().find_map(|o| match o {
            HookOutcome::Ran { response, .. } if response.r#continue == Some(false) => {
                response.message.as_deref()
            }
            _ => None,
        })
    }
    /// 第一个改写 input 的 hook 给出的新 input（按出现顺序）；多个 hook 都改写时后面的不看，记 warn
    pub fn updated_input(&self) -> Option<&serde_json::Value> {
        let mut rewrites = self.outcomes.iter().filter_map(|o| match o {
            HookOutcome::Ran { response, .. } => response.updated_input.as_ref(),
            _ => None,
        });
        let first = rewrites.next()?;
        let ignored = rewrites.count();
        if ignored > 0 {
            tracing::warn!(
                ignored,
                "several hooks rewrote the same input; only the first in configuration order applies"
            );
        }
        Some(first)
    }
}
