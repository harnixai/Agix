//! The QuickJS implementation of `ScriptEngine`, behind the `quickjs` feature.

use std::time::{Duration, Instant};

use async_trait::async_trait;
use base::interface::script::{ScriptError, ScriptLimits, ScriptSource};

/// QuickJS.
pub struct QuickJsEngine {
    /// Ceiling on a single runtime's heap. Independent of the per-call
    /// timeout: a script can exhaust memory quickly or slowly.
    memory_limit_bytes: usize,
    /// The global the base library is installed under: the engine's name.
    library: String,
}

impl Default for QuickJsEngine {
    fn default() -> Self {
        Self {
            // Generous for string work, small enough that a script cannot
            // starve the process it is a guest in.
            memory_limit_bytes: 16 * 1024 * 1024,
            library: js_lib::GLOBAL.to_string(),
        }
    }
}

impl QuickJsEngine {
    pub fn new() -> Self {
        Self::default()
    }

    /// Scripts of the engine called `engine` find the base library under
    /// that name.
    pub fn for_engine(engine: &str) -> Self {
        Self {
            library: engine.to_string(),
            ..Self::default()
        }
    }
}

/// How long past its timeout a script is waited for. The interrupt only
/// reaches JavaScript; one inside a native call ends when that call returns,
/// and its caller does not wait for that.
const ABANDON_AFTER: Duration = Duration::from_millis(100);

fn abandoned(id: &str, limits: &ScriptLimits) -> ScriptError {
    tracing::warn!(
        script = %id,
        "a script still running past its timeout was abandoned; its thread ends when the \
         native call it is in returns"
    );
    ScriptError::TimedOut {
        after: limits.timeout,
    }
}

/// Wrap the operator's code so the whole exchange is one call with strings on
/// both sides.
///
/// JSON in and JSON out rather than converting Rust values through QuickJS's
/// type system: the contract is already JSON-shaped, and going through
/// `JSON.parse` / `JSON.stringify` means a script sees exactly what a command
/// hook would see for the same point.
///
/// The user's code goes inside the function body, so `function onAssemble(…)`
/// hoists and is in scope by the time it is called.
fn wrap(code: &str, entry: &str) -> String {
    format!(
        "(function (__agix_input) {{\n\
         {code}\n\
         if (typeof {entry} !== 'function') {{\n\
           throw new Error('script does not export a function named {entry}');\n\
         }}\n\
         var __agix_out = {entry}(JSON.parse(__agix_input));\n\
         return __agix_out === undefined ? 'null' : JSON.stringify(__agix_out);\n\
         }})"
    )
}

fn run_blocking(
    code: String,
    entry: String,
    input: String,
    deadline: Instant,
    memory_limit_bytes: usize,
    library: &str,
) -> Result<String, ScriptError> {
    let runtime = rquickjs::Runtime::new().map_err(|e| ScriptError::Failed(e.to_string()))?;
    runtime.set_memory_limit(memory_limit_bytes);
    // Fires between instructions, which is what makes a script that never
    // yields stoppable. Returning true unwinds the interpreter.
    runtime.set_interrupt_handler(Some(Box::new(move || Instant::now() >= deadline)));

    let context =
        rquickjs::Context::full(&runtime).map_err(|e| ScriptError::Failed(e.to_string()))?;

    context.with(|ctx| {
        js_lib::install_named(&ctx, library).map_err(|e| script_error(&ctx, e, deadline))?;
        #[cfg(test)]
        ctx.globals()
            .set(
                "__blockFor",
                rquickjs::Function::new(ctx.clone(), |ms: u64| {
                    std::thread::sleep(Duration::from_millis(ms))
                }),
            )
            .map_err(|e| script_error(&ctx, e, deadline))?;
        let source = wrap(&code, &entry);
        let func: rquickjs::Function = ctx
            .eval(source.as_bytes())
            .map_err(|e| script_error(&ctx, e, deadline))?;
        let out: String = func
            .call((input,))
            .map_err(|e| script_error(&ctx, e, deadline))?;
        Ok(out)
    })
}

/// Turn a QuickJS error into something a caller can act on.
///
/// An interrupted script surfaces as an ordinary exception, so "did it run out
/// of time" is answered by the clock rather than by the message — QuickJS does
/// not distinguish them and a string match on its wording would break on the
/// next release.
fn script_error(ctx: &rquickjs::Ctx<'_>, e: rquickjs::Error, deadline: Instant) -> ScriptError {
    if Instant::now() >= deadline {
        return ScriptError::TimedOut {
            after: Duration::ZERO,
        };
    }
    if let rquickjs::Error::Exception = e {
        let caught = ctx.catch();
        if let Some(exception) = caught.as_exception() {
            let message = exception
                .message()
                .unwrap_or_else(|| "uncaught exception".to_string());
            return match exception.stack() {
                Some(stack) if !stack.trim().is_empty() => {
                    ScriptError::Failed(format!("{message}\n{stack}"))
                }
                _ => ScriptError::Failed(message),
            };
        }
    }
    ScriptError::Failed(e.to_string())
}

#[async_trait]
impl base::interface::script::ScriptEngine for QuickJsEngine {
    async fn eval(
        &self,
        script: &ScriptSource,
        entry: &str,
        input: serde_json::Value,
        limits: &ScriptLimits,
    ) -> Result<serde_json::Value, ScriptError> {
        let input = serde_json::to_string(&input)
            .map_err(|e| ScriptError::Failed(format!("input is not serializable: {e}")))?;
        let code = script.code.clone();
        let entry = entry.to_string();
        let id = script.id.clone();
        // The deadline is the engine's copy of the carrier's budget, because
        // the carrier's `timeout` cannot reach inside a running interpreter.
        let deadline = Instant::now() + limits.timeout;
        let memory_limit_bytes = self.memory_limit_bytes;
        let library = self.library.clone();

        // JavaScript is synchronous and CPU-bound. On an ordinary task it
        // would hold a runtime worker for its whole execution; a script that
        // spins would hold one until its deadline.
        let joined = tokio::time::timeout(
            limits.timeout + ABANDON_AFTER,
            tokio::task::spawn_blocking(move || {
                run_blocking(code, entry, input, deadline, memory_limit_bytes, &library)
            }),
        )
        .await;

        let out = match joined {
            Ok(Ok(result)) => result?,
            Err(_) => return Err(abandoned(&id, limits)),
            Ok(Err(e)) => {
                tracing::warn!(script = %id, error = %e, "script task did not finish");
                return Err(ScriptError::Failed(format!("script task failed: {e}")));
            }
        };

        serde_json::from_str(&out).map_err(|e| {
            ScriptError::Failed(format!("script returned something that is not JSON: {e}"))
        })
    }

    /// The same interpreter on a thread of its own, which the calling hook
    /// point waits for until the timeout and no longer.
    fn eval_blocking(
        &self,
        script: &ScriptSource,
        entry: &str,
        input: serde_json::Value,
        limits: &ScriptLimits,
    ) -> Result<serde_json::Value, ScriptError> {
        let input = serde_json::to_string(&input)
            .map_err(|e| ScriptError::Failed(format!("input is not serializable: {e}")))?;
        let (code, entry) = (script.code.clone(), entry.to_string());
        let deadline = Instant::now() + limits.timeout;
        let memory_limit_bytes = self.memory_limit_bytes;
        let library = self.library.clone();
        let (done, ended) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("agix-hook-script".into())
            .spawn(move || {
                let _ = done.send(run_blocking(
                    code,
                    entry,
                    input,
                    deadline,
                    memory_limit_bytes,
                    &library,
                ));
            })
            .map_err(|e| ScriptError::Failed(format!("could not start the script thread: {e}")))?;
        let out = ended
            .recv_timeout(limits.timeout + ABANDON_AFTER)
            .map_err(|_| abandoned(&script.id, limits))??;
        serde_json::from_str(&out).map_err(|e| {
            ScriptError::Failed(format!("script returned something that is not JSON: {e}"))
        })
    }
}
