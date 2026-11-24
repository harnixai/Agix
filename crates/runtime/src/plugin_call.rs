//! The tool one host call of a plugin function runs as, for a host to hand
//! to a session's [`HostToolRunner`](base::interface::plugin_call::HostToolRunner).
//! A build without `ptc` has no JavaScript to run it on.

use base::command::PluginFunction;
use std::sync::Arc;
use std::time::Duration;

/// One call of `function`.
pub struct FunctionCall<'a> {
    pub function: PluginFunction,
    /// Whether it may write inside the function's `writes`.
    pub writes: bool,
    /// At most [`time_limit`]; `None` is [`default_time`].
    pub time: Option<Duration>,
    /// The session it runs in, which the function is told; `None` for none.
    pub session_id: Option<String>,
    /// The engine name its base library is under.
    pub engine: &'a str,
    /// The slots it waits for one of; `None` when the caller already holds
    /// the call's slot.
    pub run_slots: Option<Arc<tokio::sync::Semaphore>>,
    /// The host's JavaScript libraries; the function gets those its package
    /// `uses`.
    pub host_js_libs: Arc<[base::interface::host_js::HostJsLib]>,
}

/// The longest a call may run; `None` in a build that cannot run one.
pub fn time_limit() -> Option<Duration> {
    #[cfg(feature = "ptc")]
    return Some(ptc::PluginCallTool::time_limit());
    #[cfg(not(feature = "ptc"))]
    None
}

/// How long a call runs when the host gives no time; `None` in a build that
/// cannot run one.
pub fn default_time() -> Option<Duration> {
    #[cfg(feature = "ptc")]
    return Some(ptc::PluginCallTool::default_time());
    #[cfg(not(feature = "ptc"))]
    None
}

pub fn tool(call: FunctionCall<'_>) -> Result<Arc<dyn base::tool::Tool>, String> {
    #[cfg(feature = "ptc")]
    {
        let mut tool = ptc::PluginCallTool::new(call.function, call.writes)?
            .with_library(ptc::Library::for_engine(call.engine))
            .with_host_libraries(ptc::HostLibraries::new(call.host_js_libs));
        tool = match call.run_slots {
            Some(slots) => tool.with_run_slots(slots),
            None => tool.with_slot_held(),
        };
        if let Some(time) = call.time {
            tool = tool.with_time(time)?;
        }
        if let Some(id) = call.session_id {
            tool = tool.in_session(id);
        }
        Ok(Arc::new(tool))
    }
    #[cfg(not(feature = "ptc"))]
    {
        let _ = call;
        Err("this build has no JavaScript engine to run plugin functions on".into())
    }
}
