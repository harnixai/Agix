//! The QuickJS side of a run: one thread, one single-threaded async runtime,
//! one script — the model's program, or a plugin command's entry and the call
//! into it. Tool calls leave through a channel and come back as resolved
//! promises, so `Promise.all` in the script is real concurrency on the
//! engine's own runtime.

use rquickjs::prelude::Async;
use rquickjs::{
    AsyncContext, AsyncRuntime, CatchResultExt, CaughtError, Function, Object, Promise,
};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

const PRELUDE: &str = include_str!("prelude.js");

pub(crate) struct CallRequest {
    pub name: String,
    pub input: String,
    /// `{"ok":true,"value":…}` or `{"ok":false,"error":"…"}`.
    pub reply: oneshot::Sender<String>,
}

/// What the script reaches outside itself.
#[derive(Clone)]
pub(crate) struct Host {
    pub calls: mpsc::UnboundedSender<CallRequest>,
    pub log: Arc<dyn Fn(String) + Send + Sync>,
    pub stop_requested: Arc<AtomicBool>,
    /// What only a model's program has: `agent`, `notify`, `messages`, `args`.
    pub program: Option<ProgramHost>,
}

#[derive(Clone)]
pub(crate) struct ProgramHost {
    pub notify: Arc<dyn Fn(String) + Send + Sync>,
    pub messages: Arc<Mutex<Vec<String>>>,
    pub args: String,
}

/// What the script is.
pub(crate) enum Script {
    /// The model's program, run as the body of an async function.
    Program(String),
    Command(CommandScript),
}

/// A plugin command's entry file and the call into it.
pub(crate) struct CommandScript {
    /// The entry's file name, as errors name it.
    pub filename: String,
    pub source: String,
    pub function: String,
    /// `[argv, input, ctx]` as JSON.
    pub call: String,
    /// Where the entry's bytecode is kept; `None` parses it every run.
    pub cache_dir: Option<PathBuf>,
}

impl CommandScript {
    /// The entry inside an async function, so it may `await` at its top
    /// level, resolving to the function it defines. Its own first line stays
    /// line one, so line numbers in errors match the file.
    fn wrapped(&self) -> String {
        let f = &self.function;
        format!(
            "globalThis.__agixEntry = (async () => {{ {}\n;return typeof {f} === \"undefined\" ? \
             undefined : {f}; }})();",
            self.source
        )
    }

    /// Calls the entry's function and settles to `{"text":…}` for a string,
    /// `{"value":…}` for anything else.
    fn driver(&self) -> String {
        format!(
            "(async () => {{ const f = await globalThis.__agixEntry; delete globalThis.__agixEntry; \
             if (typeof f !== \"function\") throw new Error({missing}); \
             const [argv, input, ctx] = JSON.parse({call}); \
             const r = await f(argv, input, Object.freeze(ctx)); \
             return JSON.stringify(typeof r === \"string\" ? {{ text: r }} : \
             {{ value: r === undefined ? null : r }}); }})()",
            missing = serde_json::Value::String(format!(
                "{} defines no function `{}`",
                self.filename, self.function
            )),
            call = serde_json::Value::String(self.call.clone()),
        )
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ScriptEnd {
    /// The script's return value, as JSON text.
    Returned(String),
    /// What it threw, explained.
    Threw(String),
    /// Stopped from outside: out of time or memory, or told to.
    Stopped,
}

pub(crate) struct Run {
    pub script: Script,
    pub host: Host,
    pub memory_limit: usize,
    /// [`now_ms`] after which the script is interrupted; moved later when it
    /// starts a sub-agent.
    pub deadline_ms: Arc<AtomicU64>,
    pub hard_stop: CancellationToken,
}

/// Milliseconds on a monotonic clock, so a wall-clock step neither cuts a
/// run short nor lets it run past its limit.
pub(crate) fn now_ms() -> u64 {
    static ORIGIN: std::sync::LazyLock<std::time::Instant> =
        std::sync::LazyLock::new(std::time::Instant::now);
    ORIGIN.elapsed().as_millis() as u64
}

/// Start the script on its own thread; the receiver resolves when it ends.
pub(crate) fn spawn(run: Run) -> oneshot::Receiver<ScriptEnd> {
    let (done, ended) = oneshot::channel();
    let spawned = std::thread::Builder::new()
        .name("agix-script".into())
        .spawn(move || {
            let end = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt.block_on(execute(run)),
                Err(e) => ScriptEnd::Threw(format!("could not start the script runtime: {e}")),
            };
            let _ = done.send(end);
        });
    if let Err(e) = spawned {
        let (done, ended) = oneshot::channel();
        let _ = done.send(ScriptEnd::Threw(format!(
            "could not start the script thread: {e}"
        )));
        return ended;
    }
    ended
}

async fn execute(run: Run) -> ScriptEnd {
    let hard_stop = run.hard_stop.clone();
    let end = tokio::select! {
        biased;
        _ = hard_stop.cancelled() => ScriptEnd::Stopped,
        end = evaluate(run) => end,
    };
    // A stop that lands while the script is finishing still counts: what it
    // returned may rest on calls the stop cut short.
    if hard_stop.is_cancelled() {
        ScriptEnd::Stopped
    } else {
        end
    }
}

async fn evaluate(run: Run) -> ScriptEnd {
    let runtime = match AsyncRuntime::new() {
        Ok(rt) => rt,
        Err(e) => return ScriptEnd::Threw(format!("could not create the script engine: {e}")),
    };
    runtime.set_memory_limit(run.memory_limit).await;
    let (deadline, stop) = (run.deadline_ms.clone(), run.hard_stop.clone());
    runtime
        .set_interrupt_handler(Some(Box::new(move || {
            stop.is_cancelled() || now_ms() > deadline.load(Ordering::Relaxed)
        })))
        .await;
    let context = match AsyncContext::full(&runtime).await {
        Ok(c) => c,
        Err(e) => return ScriptEnd::Threw(format!("could not create the script context: {e}")),
    };

    let host = run.host;
    let script = run.script;
    let deadline = run.deadline_ms.clone();
    let stopped = run.hard_stop.clone();
    let end = context
        .async_with(async |ctx| {
            if let Err(e) = install(&ctx, host) {
                return ScriptEnd::Threw(format!("could not set up the script globals: {e}"));
            }
            let (started, source, style) = match &script {
                Script::Program(code) => (
                    ctx.eval_with_options(wrap(code), eval_options("code.js")),
                    code.as_str(),
                    Style::Program,
                ),
                Script::Command(command) => (
                    start_command(&ctx, command),
                    command.source.as_str(),
                    Style::Command,
                ),
            };
            let promise: Promise = match started.catch(&ctx) {
                Ok(p) => p,
                Err(e) => return ScriptEnd::Threw(explain(e, source, style)),
            };
            match promise.into_future::<String>().await.catch(&ctx) {
                Ok(json) => ScriptEnd::Returned(json),
                Err(e) => ScriptEnd::Threw(explain(e, source, style)),
            }
        })
        .await;
    match end {
        ScriptEnd::Threw(_)
            if stopped.is_cancelled() || now_ms() > deadline.load(Ordering::Relaxed) =>
        {
            ScriptEnd::Stopped
        }
        other => other,
    }
}

/// Evaluate a command's entry — from bytecode this machine kept, compiling
/// and keeping it when there is none — and start the call into it.
fn start_command<'js>(
    ctx: &rquickjs::Ctx<'js>,
    command: &CommandScript,
) -> rquickjs::Result<Promise<'js>> {
    let wrapped = command.wrapped();
    match &command.cache_dir {
        Some(dir) => {
            let file = crate::bytecode::cache_file(dir, &command.filename, &wrapped);
            let kept = std::fs::read(&file).ok().and_then(|bytes| {
                match crate::bytecode::run(ctx, &bytes) {
                    crate::bytecode::Loaded::Ran(ran) => Some(ran),
                    crate::bytecode::Loaded::Unreadable => None,
                }
            });
            match kept {
                Some(ran) => ran?,
                None => {
                    let bytes = crate::bytecode::compile(ctx, &wrapped, &command.filename)?;
                    crate::bytecode::store(&file, &bytes);
                    match crate::bytecode::run(ctx, &bytes) {
                        crate::bytecode::Loaded::Ran(ran) => ran?,
                        crate::bytecode::Loaded::Unreadable => {
                            ctx.eval_with_options::<(), _>(
                                wrapped,
                                eval_options(&command.filename),
                            )?;
                        }
                    }
                }
            }
        }
        None => ctx.eval_with_options::<(), _>(wrapped, eval_options(&command.filename))?,
    }
    ctx.eval_with_options(command.driver(), eval_options("<runext>"))
}

fn eval_options(filename: &str) -> rquickjs::context::EvalOptions {
    let mut options = rquickjs::context::EvalOptions::default();
    options.global = true;
    options.strict = false;
    options.filename = Some(filename.into());
    options
}

/// The model's code inside an async function, starting on the wrapper's own
/// first line so line numbers in errors match what it wrote.
fn wrap(code: &str) -> String {
    format!(
        "(async () => {{ const __result = await (async () => {{ {code}\n}})(); \
         return JSON.stringify(__result === undefined ? null : __result); }})()"
    )
}

fn install(ctx: &rquickjs::Ctx<'_>, host: Host) -> rquickjs::Result<()> {
    let bridge = Object::new(ctx.clone())?;
    let calls = host.calls.clone();
    bridge.set(
        "call",
        Function::new(
            ctx.clone(),
            Async(move |name: String, input: String| {
                let calls = calls.clone();
                async move {
                    let (reply, answer) = oneshot::channel();
                    if calls.send(CallRequest { name, input, reply }).is_err() {
                        return Ok::<_, rquickjs::Error>(stopped_answer());
                    }
                    Ok(answer.await.unwrap_or_else(|_| stopped_answer()))
                }
            }),
        )?,
    )?;
    let log = host.log.clone();
    bridge.set(
        "log",
        Function::new(ctx.clone(), move |text: String| log(text))?,
    )?;
    let stop = host.stop_requested.clone();
    bridge.set(
        "stopRequested",
        Function::new(ctx.clone(), move || stop.load(Ordering::Relaxed))?,
    )?;
    if let Some(program) = host.program {
        let notify = program.notify.clone();
        bridge.set(
            "notify",
            Function::new(ctx.clone(), move |text: String| notify(text))?,
        )?;
        let inbox = program.messages.clone();
        bridge.set(
            "messages",
            Function::new(ctx.clone(), move || {
                let taken: Vec<String> =
                    std::mem::take(&mut *inbox.lock().unwrap_or_else(|e| e.into_inner()));
                serde_json::to_string(&taken).unwrap_or_else(|_| "[]".into())
            })?,
        )?;
        let args = program.args.clone();
        bridge.set("args", Function::new(ctx.clone(), move || args.clone())?)?;
    }
    ctx.globals().set("__host", bridge)?;
    ctx.eval::<(), _>(PRELUDE)?;
    js_lib::install(ctx)
}

fn stopped_answer() -> String {
    r#"{"ok":false,"error":"the run is stopping; no more calls are made"}"#.to_string()
}

/// Whose script an error is explained for.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Style {
    /// The model's: the message and where, since it wrote every line.
    Program,
    /// A command's: a plain `Error` is the command talking to the model, so
    /// its message alone; anything else is a defect, reported with its stack.
    Command,
}

/// An exception as the model should read it: the message, where, and what to
/// do instead when it reached for something that is not here.
fn explain(e: CaughtError<'_>, source: &str, style: Style) -> String {
    match e {
        CaughtError::Exception(ex) => {
            let message = ex.message().unwrap_or_default();
            let stack = ex.stack().unwrap_or_default();
            let name = ex.get::<_, String>("name").unwrap_or_default();
            if style == Style::Program {
                if let Some(line) = (name == "SyntaxError")
                    .then(|| module_statement_line(source, &stack))
                    .flatten()
                {
                    return format!(
                        "line {line} uses import/export, but this runs as a script: there are no \
                         modules here. Drop it and use tools.* (or callTool) for any I/O."
                    );
                }
                if message.starts_with("could not load module") {
                    return format!(
                        "{message}: there are no modules here — use tools.* (or callTool) for any I/O"
                    );
                }
            }
            if style == Style::Command && name == "Error" {
                return message;
            }
            let message = if style == Style::Command && !name.is_empty() {
                format!("{name}: {message}")
            } else {
                message
            };
            if stack.trim().is_empty() {
                message
            } else {
                format!("{message}\n{}", stack.trim_end())
            }
        }
        CaughtError::Value(v) => match v.as_string().and_then(|s| s.to_string().ok()) {
            Some(text) => text,
            None => format!("{v:?}"),
        },
        CaughtError::Error(e) => e.to_string(),
    }
}

/// The line a syntax error points at, when that line is an `import` or
/// `export` statement.
fn module_statement_line(code: &str, stack: &str) -> Option<usize> {
    let at = stack.find("code.js:")? + "code.js:".len();
    let line: usize = stack[at..]
        .split(|c: char| !c.is_ascii_digit())
        .next()?
        .parse()
        .ok()?;
    let text = code.lines().nth(line.checked_sub(1)?)?.trim_start();
    ["import", "export"]
        .iter()
        .any(|kw| {
            text.strip_prefix(kw)
                .and_then(|rest| rest.chars().next())
                .is_some_and(|c| !(c.is_alphanumeric() || c == '_' || c == '$'))
        })
        .then_some(line)
}
