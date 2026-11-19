//! The host's JavaScript libraries (`base::interface::host_js`) in plugin
//! code: which of them one run gets — those its package `uses` — the stubs
//! that stand for their functions in the script, and the Rust side of one
//! call: its limits, its cancellation, its line in the task's output and its
//! tracing span. `RunCode` runs without any.

use base::interface::host_js::{HostCallContext, HostJsError, HostJsLib};
use base::tool::ToolContext;
use serde_json::Value;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// How deep a value a host function takes or gives may nest: the limit a
/// script's tool input is parsed with.
const MAX_DEPTH: usize = 128;

/// Every library the host provides.
#[derive(Clone, Default)]
pub struct HostLibraries(Arc<[HostJsLib]>);

impl HostLibraries {
    pub fn new(libs: impl Into<Arc<[HostJsLib]>>) -> Self {
        Self(libs.into())
    }

    /// What a run of code from a package that `uses` these gets, its calls
    /// made as `context` says; `context.cancel` is replaced by the run's.
    pub(crate) fn bind(&self, uses: &[String], context: HostCallContext) -> HostBinding {
        let libs: Vec<HostJsLib> = self
            .0
            .iter()
            .filter(|lib| uses.iter().any(|u| u == lib.namespace()))
            .cloned()
            .collect();
        HostBinding {
            context: (!libs.is_empty()).then_some(context),
            libs,
        }
    }
}

/// The context a plugin command or function run from `ctx` calls the host
/// with.
pub(crate) fn call_context(
    ctx: &ToolContext,
    plugin: &str,
    plugin_version: &str,
    entry: &str,
) -> HostCallContext {
    HostCallContext {
        plugin: plugin.to_string(),
        plugin_version: plugin_version.to_string(),
        entry: entry.to_string(),
        session_id: (!ctx.session_id.is_empty()).then(|| ctx.session_id.clone()),
        project_root: Some(ctx.cwd.clone()),
        cwd: ctx.cwd.clone(),
        via: ctx.via.clone(),
        agent_depth: ctx.agent_depth,
        cancel: CancellationToken::new(),
    }
}

/// The host libraries of one run.
#[derive(Default)]
pub(crate) struct HostBinding {
    libs: Vec<HostJsLib>,
    context: Option<HostCallContext>,
}

/// What the script evaluates for its host libraries.
#[derive(Clone, Default)]
pub(crate) struct LibraryScripts {
    /// Defines each namespace with its stubs and evaluates to the function
    /// that freezes them; `None` when there are none.
    pub stubs: Option<String>,
    /// `(namespace, source)` of each library that has one, in order.
    pub preludes: Vec<(String, String)>,
}

impl HostBinding {
    pub(crate) fn scripts(&self) -> LibraryScripts {
        if self.libs.is_empty() {
            return LibraryScripts::default();
        }
        let table: Vec<(&str, Vec<&str>)> = self
            .libs
            .iter()
            .map(|lib| (lib.namespace(), lib.paths().collect()))
            .collect();
        let table = serde_json::to_string(&table).unwrap_or_else(|_| "[]".into());
        LibraryScripts {
            stubs: Some(STUBS.replace("LIBRARIES", &Value::String(table).to_string())),
            preludes: self
                .libs
                .iter()
                .filter_map(|lib| {
                    let source = lib.prelude_source()?;
                    Some((lib.namespace().to_string(), source.to_string()))
                })
                .collect(),
        }
    }

    /// One call of the function at `path` (`codix.flow.start`) with `input`
    /// as the script wrote it, given at most `max_chars` either way.
    pub(crate) async fn call(
        &self,
        path: &str,
        input: &str,
        max_chars: usize,
        cancel: CancellationToken,
    ) -> Result<Value, HostJsError> {
        let (Some(f), Some(context)) = (self.function(path), &self.context) else {
            return Err(HostJsError::new(
                "unknown",
                format!("{path} is not a host function"),
            ));
        };
        let size = input.chars().count();
        if size > max_chars {
            return Err(too_large("the input", size, max_chars));
        }
        let input: Value = serde_json::from_str(input).map_err(|e| {
            HostJsError::new("limit", format!("the input cannot be handed over: {e}"))
        })?;
        let mut context = context.clone();
        context.cancel = cancel.clone();
        // Its own task, so that a host function still running when the
        // script is stopped sees the cancellation instead of being dropped
        // mid-await; the script is not kept waiting for it.
        let answer = tokio::spawn(f.call(context, input));
        let value = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(stopped()),
            answer = answer => answer.map_err(|_| {
                HostJsError::new("failed", format!("{path} panicked"))
            })??,
        };
        if depth(&value) > MAX_DEPTH {
            return Err(HostJsError::new(
                "limit",
                format!("the result nests deeper than {MAX_DEPTH} levels"),
            ));
        }
        let size = value.to_string().chars().count();
        if size > max_chars {
            return Err(too_large("the result", size, max_chars));
        }
        Ok(value)
    }

    pub(crate) fn context(&self) -> Option<&HostCallContext> {
        self.context.as_ref()
    }

    fn function(&self, path: &str) -> Option<&Arc<dyn base::interface::host_js::HostFn>> {
        let (namespace, rest) = path.split_once('.')?;
        self.libs
            .iter()
            .find(|lib| lib.namespace() == namespace)?
            .function_at(rest)
    }
}

pub(crate) fn stopped() -> HostJsError {
    HostJsError::new("stopped", "the run is stopping; the call was cancelled")
}

fn too_large(what: &str, size: usize, max_chars: usize) -> HostJsError {
    HostJsError::new(
        "limit",
        format!(
            "{what} is {size} characters as JSON, more than the {max_chars} a host function \
             call carries"
        ),
    )
}

fn depth(value: &Value) -> usize {
    let mut deepest = 0;
    let mut open = vec![(value, 0)];
    while let Some((v, level)) = open.pop() {
        let inside = level + 1;
        match v {
            Value::Array(items) => open.extend(items.iter().map(|i| (i, inside))),
            Value::Object(map) => open.extend(map.values().map(|i| (i, inside))),
            _ => continue,
        }
        deepest = deepest.max(inside);
    }
    deepest
}

/// `LIBRARIES` is replaced by `[[namespace, [path, …]], …]` as a JSON string.
const STUBS: &str = r#"(() => {
  "use strict";
  const bridge = globalThis.__hostLibraries;
  delete globalThis.__hostLibraries;
  const { defineProperty, freeze, isFrozen, getOwnPropertyNames } = Object;
  const { parse, stringify } = JSON;
  const own = (o, k) => Object.prototype.hasOwnProperty.call(o, k);
  const call = async (path, input) => {
    const text = stringify(input === undefined ? null : input);
    if (typeof text !== "string") throw new TypeError(`${path}: the input has no JSON form`);
    const r = parse(await bridge.call(path, text));
    if (!r.ok) {
      const e = new Error(r.error);
      e.code = r.code;
      throw e;
    }
    return r.value;
  };
  const field = (o, k, value) =>
    defineProperty(o, k, { value, enumerable: true, writable: true, configurable: true });
  const roots = [];
  for (const [namespace, paths] of parse(LIBRARIES)) {
    const root = {};
    for (const path of paths) {
      const words = path.split(".");
      const last = words.pop();
      let at = root;
      for (const w of words) {
        if (!own(at, w)) field(at, w, {});
        at = at[w];
      }
      const full = `${namespace}.${path}`;
      field(at, last, (input) => call(full, input));
    }
    defineProperty(globalThis, namespace, {
      value: root, writable: false, enumerable: false, configurable: false,
    });
    roots.push(root);
  }
  const deepFreeze = (o) => {
    freeze(o);
    for (const k of getOwnPropertyNames(o)) {
      const v = o[k];
      if (((typeof v === "object" && v !== null) || typeof v === "function") && !isFrozen(v)) {
        deepFreeze(v);
      }
    }
  };
  return () => roots.forEach(deepFreeze);
})()"#;
