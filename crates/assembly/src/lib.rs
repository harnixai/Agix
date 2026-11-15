//! The assembly: how a host turns settings, modules and a working directory
//! into sessions — the session pool, provider routing, plugin packages,
//! configuration loading — and the JSON-RPC server over it (Unix socket, TCP,
//! WebSocket) with instance discovery. `embed` is the entry a host starts the
//! engine through and that `agixd` itself is a thin binary over; an embedder
//! that wants sessions without a socket uses the pool alone.
//!
//! The server lives here rather than in the binary's crate because it and the
//! pool share most of their generic instantiations (serde_json, tokio, rmcp);
//! split across two crates, each compiled its own copies, about 2.8 MB of a
//! release `agixd`.

pub mod agixd;
pub mod assemble;
pub mod config;
pub mod discovery;
pub mod doctor;
pub mod doctor_posture;
pub mod embed;
pub mod export;
pub mod health_bound;
pub mod import;
pub mod logging;
pub mod model_router;
pub mod modules;
pub mod plugins;
mod recording_view;
pub mod rpc;
pub mod server;
pub mod session_pool;
pub mod settings_sections;
mod settings_source;
#[cfg(feature = "otel")]
pub mod telemetry_otel;
pub mod watch_hub;
#[cfg(feature = "ws")]
pub mod ws;

pub use assemble::{Assembly, Transcripts};
pub use discovery::write_lock_file;
pub use server::{DaemonServer, UnixBinding};
pub use session_pool::{SessionCtx, SessionCustomizer, SessionPool};
