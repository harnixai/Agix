//! The offline cases of `tests/blackbox` — the assembled product over JSON-RPC
//! against a model stub — compiled into one test binary; each module is one
//! file of this directory. One file alone:
//! `cargo test -p blackbox --test integration daemon_surface::`.

mod audit_findings;
mod background_agents;
mod crash_recovery;
mod daemon_surface;
mod demo_plugins;
mod engine_boundaries;
mod four_project_pipeline;
mod harness;
mod hook_events;
mod host_agents;
mod mcp_boundaries;
mod minimal_tier;
mod package_trust;
mod permission_bounds;
mod project_trust_untrusted;
mod sandbox_chain;
mod session_boundaries;
mod session_prompt;
mod skills_and_memory;
mod team_lifecycle;
mod turn_throughput;
