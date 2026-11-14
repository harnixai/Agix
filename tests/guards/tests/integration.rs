//! The architecture guards, compiled into one test binary that reads the
//! workspace once; each module is one family of guards. One family alone:
//! `cargo test -p guards --test integration process_state::`.

mod api_surface;
mod architecture_doc;
mod base;
mod decorators;
mod delegation;
mod deps;
mod layering;
mod module_order;
mod permissions;
mod process_state;
mod session_state;
mod size;
mod sources;
mod support_selftest;
mod tool_pipeline;
mod transcript_writers;
mod unused_deps;
mod wiring;
