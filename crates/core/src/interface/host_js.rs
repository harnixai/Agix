//! `HostJsLib` — a JavaScript library the host gives plugin code: a namespace
//! of async functions, JSON in and JSON out, that becomes a global in the
//! runtime of every plugin command and function whose package `uses` it
//! (`docs/agix-sdk.md` §10「宿主 JS 库」). The host never sees the
//! script engine: the engine writes the stubs, and each call reaches the host
//! through [`HostFn`] with who made it and a cancellation signal.

use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;

use futures::future::BoxFuture;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::command::HostLibrary;

/// One function of a host library. Any
/// `Fn(HostCallContext, Value) -> impl Future<Output = Result<Value, HostJsError>>`
/// is one.
pub trait HostFn: Send + Sync {
    fn call(
        &self,
        cx: HostCallContext,
        input: Value,
    ) -> BoxFuture<'static, Result<Value, HostJsError>>;
}

impl<F, Fut> HostFn for F
where
    F: Fn(HostCallContext, Value) -> Fut + Send + Sync,
    Fut: Future<Output = Result<Value, HostJsError>> + Send + 'static,
{
    fn call(
        &self,
        cx: HostCallContext,
        input: Value,
    ) -> BoxFuture<'static, Result<Value, HostJsError>> {
        Box::pin(self(cx, input))
    }
}

/// Who calls a host function, from where; the host decides on it whether
/// the call goes ahead.
#[derive(Debug, Clone)]
pub struct HostCallContext {
    pub plugin: String,
    pub plugin_version: String,
    /// The command's name, or the function's.
    pub entry: String,
    pub session_id: Option<String>,
    pub project_root: Option<PathBuf>,
    pub cwd: PathBuf,
    /// Who ran the plugin code: `model`, `user` (a `/name` command),
    /// `skill`, `RunCode` or `host`.
    pub via: String,
    pub agent_depth: u32,
    /// Fires when the script is stopped; the script's promise is rejected
    /// whatever the function does after.
    pub cancel: CancellationToken,
}

/// A host function's failure, as the script's rejected `Error` carries it.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{code}: {message}")]
pub struct HostJsError {
    pub code: String,
    pub message: String,
}

impl HostJsError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

#[derive(Clone)]
pub struct HostJsLib {
    namespace: String,
    functions: Vec<(String, Arc<dyn HostFn>)>,
    prelude: Option<String>,
    description: Option<String>,
}

impl std::fmt::Debug for HostJsLib {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostJsLib")
            .field("namespace", &self.namespace)
            .field("functions", &self.paths().collect::<Vec<_>>())
            .field("prelude", &self.prelude.as_ref().map(String::len))
            .field("description", &self.description)
            .finish()
    }
}

impl HostJsLib {
    pub fn new(namespace: &str) -> Self {
        Self {
            namespace: namespace.to_string(),
            functions: Vec::new(),
            prelude: None,
            description: None,
        }
    }

    /// `path` is dot-separated JavaScript identifiers under the namespace:
    /// `flow.start` is `codix.flow.start(input)` in a script.
    pub fn function<F, Fut>(mut self, path: &str, f: F) -> Self
    where
        F: Fn(HostCallContext, Value) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Value, HostJsError>> + Send + 'static,
    {
        self.functions.push((path.to_string(), Arc::new(f)));
        self
    }

    /// Plain JavaScript evaluated after the stubs and before the plugin's
    /// code, while the namespace can still be added to; frozen after.
    pub fn prelude(mut self, js: impl Into<String>) -> Self {
        self.prelude = Some(js.into());
        self
    }

    /// What install disclosures say the library is.
    pub fn description(mut self, text: impl Into<String>) -> Self {
        self.description = Some(text.into());
        self
    }

    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    pub fn paths(&self) -> impl Iterator<Item = &str> {
        self.functions.iter().map(|(path, _)| path.as_str())
    }

    pub fn function_at(&self, path: &str) -> Option<&Arc<dyn HostFn>> {
        self.functions
            .iter()
            .find(|(p, _)| p == path)
            .map(|(_, f)| f)
    }

    pub fn prelude_source(&self) -> Option<&str> {
        self.prelude.as_deref()
    }

    /// The library as a package's `uses` is checked against.
    pub fn library(&self) -> HostLibrary {
        HostLibrary {
            name: self.namespace.clone(),
            description: self.description.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_closure_is_a_host_function() {
        let lib = HostJsLib::new("codix")
            .function("echo", |cx: HostCallContext, input| async move {
                Ok(serde_json::json!({ "via": cx.via, "input": input }))
            })
            .description("Codix flows");
        let cx = HostCallContext {
            plugin: "p".into(),
            plugin_version: "1".into(),
            entry: "cmd".into(),
            session_id: None,
            project_root: None,
            cwd: PathBuf::from("/"),
            via: "user".into(),
            agent_depth: 0,
            cancel: CancellationToken::new(),
        };
        let out = lib.function_at("echo").unwrap().call(cx, 1.into()).await;
        assert_eq!(out, Ok(serde_json::json!({ "via": "user", "input": 1 })));
        assert_eq!(
            lib.library(),
            HostLibrary {
                name: "codix".into(),
                description: Some("Codix flows".into())
            }
        );
    }
}
