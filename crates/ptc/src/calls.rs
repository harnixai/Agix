//! A script's tool calls on the engine's side: each request from the script
//! becomes a call through the session's dispatch, within the run's limits,
//! and its answer goes back to the script.

use crate::engine::CallRequest;
use crate::{Limits, StopCause};
use base::context::RunningTask;
use base::interface::tool_dispatch::ToolDispatch;
use serde_json::Value;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot, Semaphore};
use tokio_util::sync::CancellationToken;

/// One record per finished sub-call, for the error report.
#[derive(Default)]
pub(crate) struct CallLog {
    entries: Mutex<Vec<String>>,
    /// Calls that succeeded and are not read-only, in the order they ended.
    changes: Mutex<Vec<String>>,
    pub(crate) failed: AtomicU32,
    pub(crate) made: AtomicU32,
}

impl CallLog {
    /// The latest failures, and what a failed run may already have changed.
    pub(crate) fn failures(&self) -> String {
        let failures = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        if failures.is_empty() {
            return String::new();
        }
        let shown: Vec<&String> = failures.iter().rev().take(20).collect();
        format!(
            "\nFailed calls (latest {}):\n{}\nCalls that succeeded may already have changed things; TaskOutput has the full call log.",
            shown.len(),
            shown
                .into_iter()
                .rev()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join("\n")
        )
    }

    /// The calls that may have changed something, for a run that failed.
    pub(crate) fn changes(&self) -> String {
        let changes = self.changes.lock().unwrap_or_else(|e| e.into_inner());
        if changes.is_empty() {
            return String::new();
        }
        let shown: Vec<&String> = changes.iter().rev().take(50).collect();
        format!(
            "\nAlready done before it failed ({} of {}):\n{}",
            shown.len(),
            changes.len(),
            shown
                .into_iter()
                .rev()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join("\n")
        )
    }
}

/// What the script may call beyond the run's limits.
#[derive(Clone, Copy)]
pub(crate) struct Allowed {
    /// Tools the script may not call at all, and why.
    pub refused: &'static [&'static str],
    pub refusal: &'static str,
}

/// What every call of one run shares.
pub(crate) struct Dispatcher {
    pub dispatch: Arc<dyn ToolDispatch>,
    pub task: Arc<RunningTask>,
    pub limits: Limits,
    pub allowed: Allowed,
    /// Moved later when the script starts a sub-agent.
    pub deadline: Arc<AtomicU64>,
    pub started: u64,
    pub hard_stop: CancellationToken,
    pub cause: Arc<StopCause>,
    pub log: Arc<CallLog>,
}

/// What becomes of one request before it runs.
enum Admission {
    Run,
    /// Answered with this error; the script goes on.
    Refused(String),
    /// A limit is reached: answered with this error, and the run ends.
    Ended(&'static str),
}

impl Dispatcher {
    fn admit(&self, name: &str, agents: &AtomicU32) -> Admission {
        let limits = &self.limits;
        if self.allowed.refused.contains(&name) {
            return Admission::Refused(format!("{name} {}", self.allowed.refusal));
        }
        let made = self.log.made.fetch_add(1, Ordering::SeqCst) + 1;
        if made > limits.calls {
            self.cause
                .set(format!("it made more than {} tool calls", limits.calls));
            return Admission::Ended("the call limit is reached");
        }
        if name == "Agent" {
            let n = agents.fetch_add(1, Ordering::SeqCst) + 1;
            if n > limits.agents {
                self.cause
                    .set(format!("it started more than {} sub-agents", limits.agents));
                return Admission::Ended("the sub-agent limit is reached");
            }
            self.deadline.fetch_max(
                self.started + limits.time_with_agents.as_millis() as u64,
                Ordering::SeqCst,
            );
        }
        Admission::Run
    }

    pub(crate) async fn run(self, mut calls: mpsc::UnboundedReceiver<CallRequest>) {
        let this = Arc::new(self);
        let slots = Arc::new(Semaphore::new(this.limits.parallel.max(1)));
        let mut last_serial: Option<oneshot::Receiver<()>> = None;
        let agents = AtomicU32::new(0);
        let mut running = tokio::task::JoinSet::new();
        loop {
            let request = tokio::select! {
                r = calls.recv() => match r { Some(r) => r, None => break },
                _ = this.hard_stop.cancelled() => break,
            };
            let input: Value = match serde_json::from_str(&request.input) {
                Ok(v) => v,
                Err(e) => {
                    let _ = request
                        .reply
                        .send(answer_err(&format!("the input is not JSON: {e}")));
                    continue;
                }
            };
            match this.admit(&request.name, &agents) {
                Admission::Run => {}
                Admission::Refused(error) => {
                    let label = format!("{}({})", request.name, brief(&input));
                    let max = this.limits.call_result_chars;
                    let _ = request
                        .reply
                        .send(fail(&this.log, &this.task, &label, &error, max));
                    continue;
                }
                Admission::Ended(error) => {
                    this.hard_stop.cancel();
                    let _ = request.reply.send(answer_err(error));
                    break;
                }
            }
            // A call that is not concurrency-safe starts once the one submitted
            // before it has finished, and only then takes a slot: waiting in
            // line must not keep reads from running.
            let serial = !this.dispatch.concurrency_safe(&request.name, &input);
            let (turn, after) = if serial {
                let (done, next) = oneshot::channel::<()>();
                (Some(done), last_serial.replace(next))
            } else {
                (None, None)
            };
            let (this, slots) = (this.clone(), slots.clone());
            running.spawn(async move {
                let _turn = turn;
                if let Some(previous) = after {
                    let _ = previous.await;
                }
                let _slot = slots.acquire_owned().await;
                let reply = this.call(&request.name, input, serial).await;
                let _ = request.reply.send(reply);
            });
        }
        while running.join_next().await.is_some() {}
    }

    /// One admitted call, answered as the script reads it.
    async fn call(&self, name: &str, input: Value, serial: bool) -> String {
        let (task, log, max_chars) = (&self.task, &self.log, self.limits.call_result_chars);
        let label = format!("{name}({})", brief(&input));
        task.append_output(&format!("→ {label}\n"));
        let cancel = self.hard_stop.child_token();
        match self.dispatch.call(name, input, cancel).await {
            Ok(answer) if !answer.reported_failure => {
                task.append_output(&format!("← {label} ok\n"));
                if serial {
                    log.changes
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push(label);
                }
                match handed_over(answer.structured, &answer.text, max_chars) {
                    Ok(value) => serde_json::json!({"ok": true, "value": value}).to_string(),
                    Err(e) => answer_err(&e),
                }
            }
            Ok(answer) => fail(log, task, &label, &answer.text, max_chars),
            Err(e) => fail(log, task, &label, &e, max_chars),
        }
    }
}

fn fail(log: &CallLog, task: &RunningTask, label: &str, error: &str, max_chars: usize) -> String {
    log.failed.fetch_add(1, Ordering::SeqCst);
    let line = format!("← {label} error: {}", crate::first_line(error));
    task.append_output(&format!("{line}\n"));
    log.entries
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(line);
    answer_err(&cut_note(error, max_chars))
}

fn answer_err(error: &str) -> String {
    serde_json::json!({"ok": false, "error": error}).to_string()
}

/// Input fields that say what a call acts on, shown first.
const NAMES_A_TARGET: &[&str] = &["file_path", "notebook_path", "path", "command", "url"];

/// The input in a few words, for the call log.
fn brief(input: &Value) -> String {
    let text = match input {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_by_key(|k| !NAMES_A_TARGET.contains(&k.as_str()));
            keys.into_iter()
                .take(2)
                .map(|k| (k, &map[k]))
                .map(|(k, v)| {
                    let v = match v {
                        Value::String(s) => s.clone(),
                        other => other.to_string(),
                    };
                    format!("{k}: {v}")
                })
                .collect::<Vec<_>>()
                .join(", ")
        }
        other => other.to_string(),
    };
    crate::truncate(&text, 80).0
}

/// `text` cut at `max_chars`, saying so when it was.
/// What a script gets for a call that worked: the structured result when
/// there is one, else the text. A structured result longer than `max_chars`
/// as JSON keeps its shape: one that says whether it is `truncated` has its
/// `content` cut to fit, and any other is an error — a script that expects
/// an object never gets text instead.
fn handed_over(structured: Option<Value>, text: &str, max_chars: usize) -> Result<Value, String> {
    let Some(mut value) = structured else {
        return Ok(Value::String(cut_note(text, max_chars)));
    };
    let size = json_chars(&value);
    if size <= max_chars {
        return Ok(value);
    }
    let says_truncated = value.get("truncated").is_some_and(Value::is_boolean);
    match value.get("content").and_then(Value::as_str) {
        Some(content) if says_truncated => {
            let content = content.to_string();
            value["content"] = Value::String(String::new());
            value["truncated"] = Value::Bool(true);
            let room = max_chars.saturating_sub(json_chars(&value));
            value["content"] = Value::String(fitted(&content, room).to_string());
            Ok(value)
        }
        _ => Err(format!(
            "the result is {size} characters as JSON, more than the {max_chars} a script \
             gets from one call; ask for less (a range, a filter)"
        )),
    }
}

fn json_chars(value: &Value) -> usize {
    value.to_string().chars().count()
}

/// The longest start of `text` whose JSON string escape is at most `room`
/// characters, ending at a line end when it has one.
fn fitted(text: &str, room: usize) -> &str {
    let mut used = 0;
    let mut end = 0;
    for (at, c) in text.char_indices() {
        used += match c {
            '"' | '\\' | '\n' | '\r' | '\t' | '\u{8}' | '\u{c}' => 2,
            c if (c as u32) < 0x20 => 6,
            _ => 1,
        };
        if used > room {
            break;
        }
        end = at + c.len_utf8();
    }
    let kept = &text[..end];
    match kept.rfind('\n') {
        Some(line_end) if end < text.len() => &kept[..=line_end],
        _ => kept,
    }
}

pub(crate) fn cut_note(text: &str, max_chars: usize) -> String {
    base::text::truncate_chars(
        text,
        max_chars,
        &format!("\n… (cut at {max_chars} characters)"),
    )
}
