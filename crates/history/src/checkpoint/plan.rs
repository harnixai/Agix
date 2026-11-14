//! 回滚的纯函数部分：切点在日志的哪里，切点之后每个被写过的文件该恢复成什么，哪些轮次会从
//! 对话里消失，以及给宿主做回滚菜单的每轮摘要。全部只读日志，不碰文件。
//!
//! 文件按物理时间线恢复：切点之后每个路径**最早**一次写的写前内容，不论那次写所在的轮次是否
//! 已被先前的回滚作废——作废的是模型看见的过去，文件上的写真的发生过。

use super::{
    After, Before, PostRecord, PreRecord, RecordedPath, RewindBegin, RewindTarget, CHECKPOINT_NS,
    POST, PRE, REWIND_NS,
};
use crate::entry::{EnvelopedEntry, LogEntry, MessageOrigin};
use base::id::Id;
use base::message::ContentBlock;
use std::collections::{BTreeMap, HashMap};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CutError {
    NoSuchTurn(String),
    TurnRewound(String),
    NoSuchToolUse(String),
}

impl std::fmt::Display for CutError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoSuchTurn(t) => write!(f, "the session has no turn `{t}`"),
            Self::TurnRewound(t) => write!(f, "turn `{t}` was already rewound"),
            Self::NoSuchToolUse(t) => write!(f, "no file was written by tool use `{t}`"),
        }
    }
}

/// How many of `entries` a rewind to `target` keeps.
pub fn cut(
    entries: &[EnvelopedEntry],
    voided: &[bool],
    target: &RewindTarget,
) -> Result<usize, CutError> {
    let live = |i: usize| !entries[i].is_sidechain && !voided.get(i).copied().unwrap_or(false);
    match target {
        RewindTarget::ToTurn(turn) => {
            let tagged = |i: usize| entries[i].turn_id.as_deref() == Some(turn.as_str());
            match (0..entries.len()).rev().find(|&i| live(i) && tagged(i)) {
                Some(last) => Ok(last + 1),
                None if (0..entries.len()).any(tagged) => Err(CutError::TurnRewound(turn.clone())),
                None => Err(CutError::NoSuchTurn(turn.clone())),
            }
        }
        RewindTarget::ToStart(_) => Ok((0..entries.len())
            .find(|&i| live(i) && entries[i].turn_id.is_some())
            .unwrap_or(entries.len())),
        RewindTarget::ToToolUse(id) => {
            let prefix = format!("{id}.");
            entries
                .iter()
                .position(|env| {
                    pre_of(env).is_some_and(|pre| {
                        pre.tool_use_id
                            .as_deref()
                            .is_some_and(|t| t == id || t.starts_with(&prefix))
                    })
                })
                .ok_or_else(|| CutError::NoSuchToolUse(id.clone()))
        }
    }
}

/// The line a rewind keeping `keep` entries keeps last, which its `commit`
/// names.
pub fn cut_after(entries: &[EnvelopedEntry], keep: usize) -> Option<Id> {
    keep.checked_sub(1)
        .and_then(|i| entries.get(i))
        .map(|e| e.id)
}

pub fn pre_of(env: &EnvelopedEntry) -> Option<PreRecord> {
    extension_payload(env, CHECKPOINT_NS, PRE)
}

pub fn post_of(env: &EnvelopedEntry) -> Option<PostRecord> {
    extension_payload(env, CHECKPOINT_NS, POST)
}

fn extension_payload<T: serde::de::DeserializeOwned>(
    env: &EnvelopedEntry,
    namespace: &str,
    name: &str,
) -> Option<T> {
    match &env.entry {
        LogEntry::Extension { ns, event, payload } if ns == namespace && event == name => {
            serde_json::from_value(payload.clone()).ok()
        }
        _ => None,
    }
}

/// One file a rewind puts back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileStep {
    pub path: RecordedPath,
    /// What it was before the first write after the cut.
    pub restore: Before,
    /// What the engine last wrote it to; `None` when that write has no
    /// `post`, so nobody knows what it should be now.
    pub expected_now: Option<After>,
    /// The tools that wrote it after the cut, in order.
    pub tools: Vec<String>,
}

/// Every file written after the first `keep` entries, by path.
pub fn file_steps(entries: &[EnvelopedEntry], keep: usize) -> Vec<FileStep> {
    let mut posts: HashMap<String, After> = HashMap::new();
    for env in entries.iter().skip(keep) {
        if let Some(post) = post_of(env) {
            if let Some(after) = post.after.into_iter().next() {
                posts.insert(post.pre, after);
            }
        }
    }
    let mut steps: BTreeMap<RecordedPath, (FileStep, String)> = BTreeMap::new();
    for env in entries.iter().skip(keep) {
        let Some(pre) = pre_of(env) else { continue };
        for file in pre.files {
            match steps.get_mut(&file.path) {
                Some((step, latest)) => {
                    step.tools.push(pre.tool.clone());
                    *latest = pre.id.clone();
                }
                None => {
                    steps.insert(
                        file.path.clone(),
                        (
                            FileStep {
                                path: file.path,
                                restore: file.before,
                                expected_now: None,
                                tools: vec![pre.tool.clone()],
                            },
                            pre.id.clone(),
                        ),
                    );
                }
            }
        }
    }
    steps
        .into_values()
        .map(|(mut step, latest)| {
            step.expected_now = posts.remove(&latest);
            step
        })
        .collect()
}

/// A turn as a rewind menu lists it.
#[derive(Debug, Clone, PartialEq)]
pub struct TurnSummary {
    pub turn_id: String,
    pub started_at: time::OffsetDateTime,
    /// The message the turn was started with.
    pub prompt: Vec<ContentBlock>,
    /// Written during the turn — by the root engine, or by a delegate while
    /// it was the latest turn — in order.
    pub files: Vec<TurnWrite>,
    /// Where its lines start and end in the log.
    pub first: usize,
    pub last: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnWrite {
    pub path: RecordedPath,
    pub tool: String,
    pub agent: Option<String>,
}

/// The turns still in the conversation, in order.
pub fn live_turns(entries: &[EnvelopedEntry], voided: &[bool]) -> Vec<TurnSummary> {
    let mut turns: Vec<TurnSummary> = Vec::new();
    let mut at: HashMap<String, usize> = HashMap::new();
    for (i, env) in entries.iter().enumerate() {
        if env.is_sidechain || voided.get(i).copied().unwrap_or(false) {
            continue;
        }
        if let Some(turn) = &env.turn_id {
            let slot = *at.entry(turn.clone()).or_insert_with(|| {
                turns.push(TurnSummary {
                    turn_id: turn.clone(),
                    started_at: env.ts,
                    prompt: Vec::new(),
                    files: Vec::new(),
                    first: i,
                    last: i,
                });
                turns.len() - 1
            });
            let summary = &mut turns[slot];
            summary.last = i;
            if summary.prompt.is_empty() {
                if let (
                    LogEntry::User { content },
                    Some(MessageOrigin::User | MessageOrigin::Command),
                ) = (&env.entry, env.origin)
                {
                    summary.prompt = content.clone();
                }
            }
        }
        if let Some(pre) = pre_of(env) {
            let owner = match &env.turn_id {
                Some(turn) => at.get(turn).copied(),
                None => turns.len().checked_sub(1),
            };
            if let Some(owner) = owner {
                for file in pre.files {
                    turns[owner].files.push(TurnWrite {
                        path: file.path,
                        tool: pre.tool.clone(),
                        agent: pre.agent.as_ref().map(|a| a.session_id.clone()),
                    });
                }
            }
        }
    }
    turns
}

/// A rewind that began and neither committed nor aborted: the process ended
/// in the middle of it.
pub fn interrupted_rewind(entries: &[EnvelopedEntry]) -> Option<(String, time::OffsetDateTime)> {
    let mut open: Option<(String, time::OffsetDateTime)> = None;
    for env in entries {
        let LogEntry::Extension { ns, event, payload } = &env.entry else {
            continue;
        };
        if ns != REWIND_NS {
            continue;
        }
        let id = payload.get("rewind_id").and_then(|v| v.as_str());
        match event.as_str() {
            super::BEGIN => {
                if let Ok(begin) = serde_json::from_value::<RewindBegin>(payload.clone()) {
                    open = Some((begin.rewind_id, env.ts));
                }
            }
            _ if open.as_ref().map(|(o, _)| o.as_str()) == id => open = None,
            _ => {}
        }
    }
    open
}

#[cfg(test)]
#[path = "plan_tests.rs"]
mod tests;
