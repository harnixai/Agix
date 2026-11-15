//! The fixture other crates' tests build a `ToolContext` from: a test session
//! over the providers the test hands in.

use base::context::{EngineConfig, Session};
use base::interface::exec::{ExecProviders, SandboxSettings};
use base::tool::ToolContext;
use std::path::PathBuf;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

use crate::SessionState;

pub fn tool_context_for_test(exec: ExecProviders, cwd: PathBuf) -> ToolContext {
    let mut config = EngineConfig::defaults_for("test");
    config.sandbox = Arc::new(SandboxSettings {
        disabled: true,
        ..Default::default()
    });
    config.file_limits.max_file_read_bytes = 0;
    let session: Arc<dyn Session> = Arc::new(SessionState::for_tests(cwd.clone()));
    let mut ctx = ToolContext::new(
        cwd,
        CancellationToken::new(),
        exec,
        Arc::new(config),
        session,
    );
    ctx.session_id = "test".into();
    ctx
}
