//! `session.rerunCall` 在录像视图一侧的两半：把宿主的改动套到从录像读回的请求上，以及把重发的
//! 结局投影成协议形状——`response` 与 `session.call` 同形，`diff` 取自
//! `telemetry::recorder::rerun::diff`。

use super::{call_error, call_usage, Response};
use crate::session_pool::api::{
    ArgDiffView, BlockSelector, CallResponse, CallStatus, RerunDiff, RerunEdits, RerunResult,
    StopReasonDiff, SystemEdit, TextDiff, ToolStepDiff, ToolStepStatus, ToolUsesDiff, UsageDiff,
};
use base::interface::model::{ModelErrorKind, ModelEvent, Usage};
use base::prompt::PromptBlock;
use serde_json::Value;
use telemetry::recorder::format::RecordedError;
use telemetry::recorder::rerun::{LoadedRequest, Rerun, ResponseDiff, ToolStep};

/// `request` with `edits` made, or why they cannot be.
pub(crate) fn apply_edits(request: &mut LoadedRequest, edits: &RerunEdits) -> Result<(), String> {
    let blocks = std::mem::take(&mut request.prompt_blocks);
    request.prompt_blocks = edit_system(blocks, &edits.system)?;
    drop_messages(request, &edits.drop_messages)
}

fn edit_system(blocks: Vec<PromptBlock>, edits: &[SystemEdit]) -> Result<Vec<PromptBlock>, String> {
    let mut plan: Vec<Option<&SystemEdit>> = vec![None; blocks.len()];
    for (at, edit) in edits.iter().enumerate() {
        let target = resolve(&blocks, at, &edit.block)?;
        if plan[target].replace(edit).is_some() {
            return Err(format!(
                "edits.system[{at}]: system block {target} is already edited by an earlier entry"
            ));
        }
    }
    Ok(blocks
        .into_iter()
        .zip(plan)
        .filter_map(|(mut block, edit)| match edit {
            None => Some(block),
            Some(SystemEdit { text: None, .. }) => None,
            Some(SystemEdit {
                text: Some(text), ..
            }) => {
                block.content = text.clone();
                Some(block)
            }
        })
        .collect())
}

fn resolve(blocks: &[PromptBlock], at: usize, selector: &BlockSelector) -> Result<usize, String> {
    match selector {
        BlockSelector::Index(i) if *i < blocks.len() => Ok(*i),
        BlockSelector::Index(i) => Err(format!(
            "edits.system[{at}]: index {i} is past the call's {} system block(s)",
            blocks.len()
        )),
        BlockSelector::Source(name) => {
            let named: Vec<usize> = blocks
                .iter()
                .enumerate()
                .filter(|(_, b)| b.name.as_deref() == Some(name.as_str()))
                .map(|(i, _)| i)
                .collect();
            match named.as_slice() {
                [one] => Ok(*one),
                [] => Err(format!(
                    "edits.system[{at}]: the call has no system block named `{name}`"
                )),
                many => Err(format!(
                    "edits.system[{at}]: {} system blocks are named `{name}`; address one by index",
                    many.len()
                )),
            }
        }
    }
}

/// The messages at `drop` taken out. The turn's input map points into the
/// message list by position, so it follows its message or goes with it.
fn drop_messages(request: &mut LoadedRequest, drop: &[usize]) -> Result<(), String> {
    if drop.is_empty() {
        return Ok(());
    }
    let len = request.messages.len();
    let mut gone = vec![false; len];
    for &n in drop {
        match gone.get_mut(n) {
            None => {
                return Err(format!(
                    "drop_messages: {n} is past the call's {len} message(s)"
                ))
            }
            Some(true) => return Err(format!("drop_messages: {n} is listed twice")),
            Some(slot) => *slot = true,
        }
    }
    if gone.iter().all(|g| *g) {
        return Err("drop_messages: dropping every message leaves nothing to send".into());
    }
    request.params.input_map = request.params.input_map.take().and_then(|mut map| {
        let kept = gone.get(map.user_message) == Some(&false);
        kept.then(|| {
            map.user_message -= gone[..map.user_message].iter().filter(|g| **g).count();
            map
        })
    });
    let mut flags = gone.into_iter();
    request.messages.retain(|_| !flags.next().unwrap_or(false));
    Ok(())
}

/// The usage a response's closing event reports.
pub(crate) fn end_usage(events: &[ModelEvent]) -> Option<Usage> {
    events.iter().rev().find_map(|e| match e {
        ModelEvent::EndTurn { usage, .. } => Some(*usage),
        _ => None,
    })
}

pub(crate) struct RerunOf<'a> {
    pub(crate) session_id: &'a str,
    pub(crate) index: usize,
    pub(crate) generation: u64,
    pub(crate) recorded_usage: Option<Usage>,
}

pub(crate) fn rerun_result(of: RerunOf<'_>, rerun: &Rerun) -> RerunResult {
    let (provider, model) = match &rerun.served {
        Some((provider, model)) => (Some(provider.clone()), Some(model.clone())),
        None => (None, None),
    };
    let live_usage = end_usage(&rerun.live);
    RerunResult {
        session_id: of.session_id.to_string(),
        index: of.index,
        generation: of.generation,
        provider,
        model,
        response: live_response(rerun, live_usage),
        diff: diff_view(&rerun.diff, of.recorded_usage, live_usage),
    }
}

fn live_response(rerun: &Rerun, usage: Option<Usage>) -> CallResponse {
    let response = Response::of(&rerun.live);
    CallResponse {
        status: match &rerun.error {
            None => CallStatus::Ok,
            Some(e) if e.kind == ModelErrorKind::Cancelled => CallStatus::Cancelled,
            Some(_) => CallStatus::Error,
        },
        text: response.text,
        thinking_chars: response.thinking_chars,
        tool_uses: response.tool_uses,
        stop_reason: rerun.diff.tools.live_stop_reason.clone(),
        usage: usage.map(call_usage),
        ttft_ms: rerun.ttft.map(millis),
        duration_ms: Some(millis(rerun.duration)),
        error: rerun
            .error
            .as_ref()
            .map(|e| call_error(&RecordedError::from(e))),
    }
}

fn millis(d: std::time::Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

fn diff_view(diff: &ResponseDiff, recorded: Option<Usage>, live: Option<Usage>) -> RerunDiff {
    let tools = &diff.tools;
    RerunDiff {
        same: diff.identical(),
        text: TextDiff {
            same: diff.text_identical(),
            recorded: diff.recorded_text.clone(),
            live: diff.live_text.clone(),
        },
        tool_uses: ToolUsesDiff {
            same: tools.steps.iter().all(ToolStep::matches),
            steps: tools.steps.iter().map(step_view).collect(),
        },
        stop_reason: StopReasonDiff {
            same: tools.recorded_stop_reason == tools.live_stop_reason,
            recorded: tools.recorded_stop_reason.clone(),
            live: tools.live_stop_reason.clone(),
        },
        usage: UsageDiff {
            recorded: recorded.map(call_usage),
            live: live.map(call_usage),
        },
    }
}

fn step_view(step: &ToolStep) -> ToolStepDiff {
    match step {
        ToolStep::Same { name } => ToolStepDiff {
            status: ToolStepStatus::Same,
            recorded: Some(name.clone()),
            live: Some(name.clone()),
            args: Vec::new(),
        },
        ToolStep::ArgsDiffer { name, args } => ToolStepDiff {
            status: ToolStepStatus::ArgsDiffer,
            recorded: Some(name.clone()),
            live: Some(name.clone()),
            args: args
                .iter()
                .map(|a| ArgDiffView {
                    key: a.key.clone(),
                    same: a.matches(),
                    recorded: a.recorded.as_deref().map(json),
                    live: a.live.as_deref().map(json),
                })
                .collect(),
        },
        ToolStep::NameDiffers { recorded, live } => ToolStepDiff {
            status: ToolStepStatus::NameDiffers,
            recorded: recorded.clone(),
            live: live.clone(),
            args: Vec::new(),
        },
    }
}

/// An argument as the diff holds it — its canonical JSON — back as a value.
fn json(canonical: &str) -> Value {
    serde_json::from_str(canonical).unwrap_or_else(|_| Value::String(canonical.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use base::interface::model::{InputMap, MessageRole, ModelContentBlock, ModelMessage};
    use base::interface::model::{StreamParams, ThinkingMode};
    use telemetry::recorder::blob::BlobId;
    use telemetry::recorder::format::{CallRecord, RecordedParams};

    fn named(name: &str, text: &str) -> PromptBlock {
        let mut block = PromptBlock::system(text);
        block.name = Some(name.to_string());
        block
    }

    fn message(text: &str) -> ModelMessage {
        ModelMessage {
            role: MessageRole::User,
            content: vec![ModelContentBlock::Text { text: text.into() }],
        }
    }

    fn request() -> LoadedRequest {
        LoadedRequest {
            prompt_blocks: vec![
                named("identity", "I am"),
                named("rules", "rule one"),
                named("dup", "a"),
                named("dup", "b"),
            ],
            tools: vec![],
            messages: vec![message("m0"), message("m1"), message("m2")],
            params: StreamParams {
                model: "m".into(),
                max_tokens: 10,
                thinking_mode: ThinkingMode::Off,
                effort: None,
                cache_edits: vec![],
                origin: None,
                input_map: Some(InputMap {
                    user_message: 2,
                    spans: vec![],
                }),
                provider_search: None,
                usage_tap: Default::default(),
            },
            recorded_response: vec![],
            record: CallRecord {
                seq: 0,
                ts: 0,
                session_id: None,
                parent_session_id: None,
                agent_type: None,
                turn_id: None,
                turn: 0,
                step: 0,
                purpose: None,
                provider: "p".into(),
                api_type: base::provider::ApiType::Anthropic,
                params: RecordedParams {
                    model: "m".into(),
                    max_tokens: 10,
                    thinking_mode: ThinkingMode::Off,
                    effort: None,
                    cache_edits: vec![],
                    search_max_uses: None,
                },
                system: vec![],
                tools: BlobId(String::new()),
                messages: vec![],
                input_map: None,
            },
        }
    }

    fn edit(block: BlockSelector, text: Option<&str>) -> SystemEdit {
        SystemEdit {
            block,
            text: text.map(str::to_string),
        }
    }

    fn texts(request: &LoadedRequest) -> Vec<&str> {
        request
            .prompt_blocks
            .iter()
            .map(|b| b.content.as_str())
            .collect()
    }

    #[test]
    fn edits_address_the_recorded_positions_not_the_shifted_ones() {
        let mut r = request();
        let edits = RerunEdits {
            system: vec![
                edit(BlockSelector::Index(0), None),
                edit(BlockSelector::Index(1), Some("rule two")),
                edit(BlockSelector::Source("identity".into()), None),
            ],
            drop_messages: vec![],
        };
        let refused = apply_edits(&mut r, &edits).unwrap_err();
        assert!(refused.contains("edits.system[2]"), "{refused}");

        let mut r = request();
        let edits = RerunEdits {
            system: vec![
                edit(BlockSelector::Index(0), None),
                edit(BlockSelector::Source("rules".into()), Some("rule two")),
            ],
            drop_messages: vec![],
        };
        apply_edits(&mut r, &edits).unwrap();
        assert_eq!(texts(&r), ["rule two", "a", "b"]);
    }

    #[test]
    fn a_source_must_name_exactly_one_block() {
        for (source, says) in [("absent", "no system block"), ("dup", "2 system blocks")] {
            let mut r = request();
            let edits = RerunEdits {
                system: vec![edit(BlockSelector::Source(source.into()), Some("x"))],
                drop_messages: vec![],
            };
            let refused = apply_edits(&mut r, &edits).unwrap_err();
            assert!(refused.contains(says), "{refused}");
        }
        let mut r = request();
        let edits = RerunEdits {
            system: vec![edit(BlockSelector::Index(4), Some("x"))],
            drop_messages: vec![],
        };
        assert!(apply_edits(&mut r, &edits).unwrap_err().contains("past"));
    }

    #[test]
    fn dropping_messages_moves_the_input_map_with_its_message() {
        let mut r = request();
        let edits = RerunEdits {
            system: vec![],
            drop_messages: vec![0],
        };
        apply_edits(&mut r, &edits).unwrap();
        assert_eq!(r.messages.len(), 2);
        assert_eq!(r.params.input_map.as_ref().unwrap().user_message, 1);

        let mut r = request();
        let edits = RerunEdits {
            system: vec![],
            drop_messages: vec![2],
        };
        apply_edits(&mut r, &edits).unwrap();
        assert!(r.params.input_map.is_none());
    }

    #[test]
    fn bad_message_drops_are_refused() {
        for (drop, says) in [
            (vec![3], "past"),
            (vec![1, 1], "twice"),
            (vec![0, 1, 2], "nothing to send"),
        ] {
            let mut r = request();
            let edits = RerunEdits {
                system: vec![],
                drop_messages: drop,
            };
            let refused = apply_edits(&mut r, &edits).unwrap_err();
            assert!(refused.contains(says), "{refused}");
        }
    }
}
