//! The offline cases of `tests/blackbox` — the assembled product over JSON-RPC
//! against a model stub — compiled into one test binary; each module is one
//! file of this directory. One file alone:
//! `cargo test -p blackbox --test integration daemon_surface::`.

mod background_agents;
mod crash_recovery;
mod daemon_surface;
mod demo_plugins;
mod embed_engine_name;
mod embed_entry;
mod embed_host_libraries;
mod embed_product_name;
mod embed_project_dir;
mod engine_boundaries;
mod engine_proxy;
mod forge;
mod four_project_pipeline;
mod harness;
mod hook_events;
mod host_agents;
mod host_skill_dirs;
mod hostile_plugins;
mod import_sources;
mod large_tool_results;
mod mcp_boundaries;
mod mcp_elicitation;
mod mcp_project_root;
mod mcp_tasks;
mod minimal_tier;
mod openai_responses;
mod other_instances;
mod package_trust;
mod permission_bounds;
mod plugin_calls;
mod plugin_commands;
mod plugin_functions;
mod project_state;
mod project_trust_untrusted;
mod provider_tiers;
mod sandbox_chain;
mod session_attended;
mod session_boundaries;
mod session_calls;
mod session_permission_mode;
mod session_prompt;
mod session_rewind;
mod settings_take_effect;
mod skills_and_memory;
mod stream_recovery;
mod team_lifecycle;
mod turn_throughput;
mod worktree_location;
