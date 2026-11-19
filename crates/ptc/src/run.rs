//! One script from admission to how it ended, shared by `RunCode` and
//! `RunExt`: takes a run slot, wires the host the script talks to, runs the
//! engine beside the call dispatcher, and watches the clock, the turn and a
//! stop. Each tool turns the ending into what the model reads.

use crate::calls::{Allowed, CallLog, Dispatcher};
use crate::engine::{self, Host, ProgramHost, Run, Script, ScriptEnd};
use crate::host_libs::HostBinding;
use crate::{take_one, Library, Limits, StopCause};
use base::context::{RunningTask, Session};
use base::interface::tool_dispatch::ToolDispatch;
use serde_json::Value;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, Semaphore};
use tokio_util::sync::CancellationToken;

/// What runs: the model's program, a plugin command, or a plugin function
/// the host called.
pub(crate) enum Front {
    Program {
        code: String,
        args: Option<Value>,
        background: bool,
    },
    Command(engine::CommandScript),
    Function(engine::CommandScript),
}

/// Where a script runs and what holds it back.
pub(crate) struct Launch<'a> {
    pub dispatch: Arc<dyn ToolDispatch>,
    pub task: &'a Arc<RunningTask>,
    pub session: &'a Arc<dyn Session>,
    pub limits: &'a Limits,
    pub library: &'a Library,
    pub allowed: Allowed,
    pub run_slots: Option<&'a Arc<Semaphore>>,
    pub turn_cancel: CancellationToken,
    /// The host libraries the script's package `uses`; none for `RunCode`.
    pub host_libraries: HostBinding,
}

/// How a script ended, with what it did on the way.
pub(crate) struct Ended {
    pub end: ScriptEnd,
    pub calls: Arc<CallLog>,
    pub log: Arc<Mutex<ProgramLog>>,
    pub cause: Arc<StopCause>,
    pub deadline: Arc<AtomicU64>,
}

impl Ended {
    /// Why a stopped script stopped.
    pub(crate) fn why_stopped(&self) -> String {
        self.cause.get().unwrap_or_else(|| {
            if engine::now_ms() >= self.deadline.load(Ordering::Relaxed) {
                "it ran out of time".to_string()
            } else {
                "it was stopped".to_string()
            }
        })
    }

    pub(crate) fn log_tail(&self) -> String {
        self.log.lock().unwrap_or_else(|e| e.into_inner()).tail()
    }
}

/// `Err` says why it did not start.
pub(crate) async fn execute(front: Front, launch: Launch<'_>) -> Result<Ended, String> {
    let Launch {
        dispatch,
        task,
        session,
        limits,
        library,
        allowed,
        run_slots,
        turn_cancel,
        host_libraries,
    } = launch;
    let _slot = acquire_run_slot(run_slots, &turn_cancel, task).await?;
    let started = engine::now_ms();
    let deadline = Arc::new(AtomicU64::new(started + limits.time.as_millis() as u64));
    let hard_stop = CancellationToken::new();
    let stop_requested = Arc::new(AtomicBool::new(false));
    let cause = Arc::new(StopCause::default());
    let calls_log = Arc::new(CallLog::default());

    let watchers = CancellationToken::new();
    spawn_watchers(
        task,
        &turn_cancel,
        &deadline,
        &hard_stop,
        &stop_requested,
        &cause,
        &watchers,
        limits,
    );

    let (calls_tx, calls_rx) = mpsc::unbounded_channel();
    let program_log = Arc::new(Mutex::new(ProgramLog::new(limits.log_chars)));
    let (script, program) = script_of(front, task, session, &program_log, limits);
    let log_task = task.clone();
    let log_buffer = program_log.clone();
    let host = Host {
        calls: calls_tx,
        log: Arc::new(move |text: String| {
            log_task.append_output(&format!("{text}\n"));
            log_buffer
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(&text);
        }),
        stop_requested: stop_requested.clone(),
        program,
        library: library.global().to_string(),
        host_libraries: host_libraries.scripts(),
    };

    let dispatcher = tokio::spawn(
        Dispatcher {
            dispatch,
            task: task.clone(),
            limits: limits.clone(),
            allowed,
            deadline: deadline.clone(),
            started,
            hard_stop: hard_stop.clone(),
            cause: cause.clone(),
            log: calls_log.clone(),
            host_libraries: Arc::new(host_libraries),
        }
        .run(calls_rx),
    );

    let ended = engine::spawn(Run {
        script,
        host,
        memory_limit: limits.memory_bytes,
        deadline_ms: deadline.clone(),
        hard_stop: hard_stop.clone(),
    });
    let end = until_cut_off(ended, &hard_stop, task).await;
    hard_stop.cancel();
    watchers.cancel();
    wind_down(dispatcher, task, limits).await;
    Ok(Ended {
        end,
        calls: calls_log,
        log: program_log,
        cause,
        deadline,
    })
}

/// How long a script that was cut off has to stop before it is left behind.
/// The interrupt only reaches JavaScript: one inside a native call stops
/// when that call returns, which may be never, and until then its caller
/// and its run slot would wait with it.
const ABANDON_AFTER: Duration = Duration::from_secs(1);

async fn until_cut_off(
    mut ended: oneshot::Receiver<ScriptEnd>,
    hard_stop: &CancellationToken,
    task: &RunningTask,
) -> ScriptEnd {
    tokio::select! {
        end = &mut ended => return end.unwrap_or(ScriptEnd::Stopped),
        _ = hard_stop.cancelled() => {}
    }
    match tokio::time::timeout(ABANDON_AFTER, ended).await {
        Ok(end) => end.unwrap_or(ScriptEnd::Stopped),
        Err(_) => {
            tracing::warn!(
                task = %task.task_id,
                "a script still running {}s after it was cut off was abandoned; its thread \
                 ends when the native call it is in returns",
                ABANDON_AFTER.as_secs()
            );
            ScriptEnd::Stopped
        }
    }
}

/// Give the sub-calls still running the grace period, then abandon them.
async fn wind_down(
    mut dispatcher: tokio::task::JoinHandle<()>,
    task: &RunningTask,
    limits: &Limits,
) {
    if tokio::time::timeout(limits.stop_grace, &mut dispatcher)
        .await
        .is_err()
    {
        // Dropping the dispatcher's JoinSet aborts every sub-call still in it.
        dispatcher.abort();
        task.append_output(&format!(
            "(sub-calls still running {}s after the script ended were abandoned)\n",
            limits.stop_grace.as_secs()
        ));
    }
    task.report_progress();
}

fn script_of(
    front: Front,
    task: &Arc<RunningTask>,
    session: &Arc<dyn Session>,
    program_log: &Arc<Mutex<ProgramLog>>,
    limits: &Limits,
) -> (Script, Option<ProgramHost>) {
    match front {
        Front::Program {
            code,
            args,
            background,
        } => {
            let messages = Arc::new(Mutex::new(Vec::new()));
            let _ = task.inbox.set(Arc::new(CodeInbox(messages.clone())));
            let program = program_host(
                task,
                session,
                program_log,
                messages,
                limits.notices,
                background,
                args.as_ref(),
            );
            (Script::Program(code), Some(program))
        }
        Front::Command(command) | Front::Function(command) => (Script::Command(command), None),
    }
}

async fn acquire_run_slot(
    run_slots: Option<&Arc<Semaphore>>,
    turn_cancel: &CancellationToken,
    task: &RunningTask,
) -> Result<Option<tokio::sync::OwnedSemaphorePermit>, String> {
    let Some(slots) = run_slots else {
        return Ok(None);
    };
    tokio::select! {
        slot = slots.clone().acquire_owned() => match slot {
            Ok(slot) => Ok(Some(slot)),
            Err(_) => Err("no script can run in this process any more".into()),
        },
        _ = turn_cancel.cancelled() => Err("the turn was interrupted".into()),
        _ = task.cancel.cancelled() => Err("it was stopped".into()),
    }
}

fn program_host(
    task: &Arc<RunningTask>,
    session: &Arc<dyn Session>,
    program_log: &Arc<Mutex<ProgramLog>>,
    messages: Arc<Mutex<Vec<String>>>,
    notices: u32,
    background: bool,
    args: Option<&Value>,
) -> ProgramHost {
    let notify_log = program_log.clone();
    let notices_left = Arc::new(AtomicU32::new(notices));
    let (notify_task, notify_session) = (task.clone(), session.clone());
    ProgramHost {
        notify: Arc::new(move |text: String| {
            notify_task.append_output(&format!("[notify] {text}\n"));
            if !background {
                notify_log
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(&text);
            } else if take_one(&notices_left) {
                notify_session.post_notice(
                    None,
                    format!(
                        "<task-update>\n<task-id>{}</task-id>\n{text}\n</task-update>",
                        notify_task.task_id
                    ),
                );
            }
        }),
        messages,
        args: args.map(Value::to_string).unwrap_or_else(|| "{}".into()),
    }
}

/// What the script logged, keeping only as much of the end as the result
/// can show.
pub(crate) struct ProgramLog {
    pub(crate) text: String,
    dropped: bool,
    keep_chars: usize,
}

impl ProgramLog {
    pub(crate) fn new(keep_chars: usize) -> Self {
        Self {
            text: String::new(),
            dropped: false,
            keep_chars,
        }
    }

    pub(crate) fn push(&mut self, line: &str) {
        self.text.push_str(line);
        self.text.push('\n');
        let keep_bytes = self.keep_chars.saturating_mul(4).max(1);
        if self.text.len() > keep_bytes.saturating_mul(2) {
            let mut cut = self.text.len() - keep_bytes;
            while !self.text.is_char_boundary(cut) {
                cut += 1;
            }
            self.text.drain(..cut);
            self.dropped = true;
        }
    }

    /// The end of it, for the result.
    pub(crate) fn tail(&self) -> String {
        log_tail(&self.text, self.keep_chars, self.dropped)
    }
}

pub(crate) fn log_tail(log: &str, max_chars: usize, dropped: bool) -> String {
    if log.trim().is_empty() {
        return String::new();
    }
    let total = log.chars().count();
    let (tail, note) = if total > max_chars || dropped {
        (
            log.chars()
                .skip(total.saturating_sub(max_chars))
                .collect::<String>(),
            format!(" (last {max_chars} characters)"),
        )
    } else {
        (log.to_string(), String::new())
    };
    format!("\nLog{note}:\n{}", tail.trim_end())
}

struct CodeInbox(Arc<Mutex<Vec<String>>>);

impl std::fmt::Debug for CodeInbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CodeInbox")
    }
}

impl base::context::TaskInbox for CodeInbox {
    fn deliver(&self, message: String) -> Result<(), String> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(message);
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
fn spawn_watchers(
    task: &Arc<RunningTask>,
    turn_cancel: &CancellationToken,
    deadline: &Arc<AtomicU64>,
    hard_stop: &CancellationToken,
    stop_requested: &Arc<AtomicBool>,
    cause: &Arc<StopCause>,
    done: &CancellationToken,
    limits: &Limits,
) {
    {
        let (task_cancel, hard_stop, stop_requested, cause, done, grace) = (
            task.cancel.clone(),
            hard_stop.clone(),
            stop_requested.clone(),
            cause.clone(),
            done.clone(),
            limits.stop_grace,
        );
        tokio::spawn(async move {
            tokio::select! {
                _ = done.cancelled() => return,
                _ = task_cancel.cancelled() => {}
            }
            stop_requested.store(true, Ordering::SeqCst);
            tokio::select! {
                _ = done.cancelled() => {}
                _ = tokio::time::sleep(grace) => {
                    cause.set(format!("it was asked to stop and did not wrap up within {}s", grace.as_secs()));
                    hard_stop.cancel();
                }
            }
        });
    }
    {
        let (turn_cancel, hard_stop, cause, done) = (
            turn_cancel.clone(),
            hard_stop.clone(),
            cause.clone(),
            done.clone(),
        );
        tokio::spawn(async move {
            tokio::select! {
                _ = done.cancelled() => {}
                _ = turn_cancel.cancelled() => {
                    cause.set("the turn was interrupted");
                    hard_stop.cancel();
                }
            }
        });
    }
    {
        let (deadline, hard_stop, cause, done) = (
            deadline.clone(),
            hard_stop.clone(),
            cause.clone(),
            done.clone(),
        );
        tokio::spawn(async move {
            loop {
                let left = deadline
                    .load(Ordering::Relaxed)
                    .saturating_sub(engine::now_ms());
                if left == 0 {
                    cause.set("it ran out of time");
                    hard_stop.cancel();
                    return;
                }
                tokio::select! {
                    _ = done.cancelled() => return,
                    _ = tokio::time::sleep(Duration::from_millis(left.min(1_000))) => {}
                }
            }
        });
    }
    {
        let (task, done) = (task.clone(), done.clone());
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = done.cancelled() => return,
                    _ = tokio::time::sleep(Duration::from_secs(1)) => task.report_progress(),
                }
            }
        });
    }
}
