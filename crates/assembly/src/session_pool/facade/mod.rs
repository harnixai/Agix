//! `impl SessionPool` 的各个部分：池对外的方法，按主题分文件。门面编排协作者
//! （`parts/`），协作者之间的配合都在这里发生。

mod background;
mod bindings;
mod config_explain;
mod config_set;
mod create;
mod doctor;
mod given_id;
mod goal;
pub(super) mod helpers;
pub(super) mod history_rpc;
pub(super) mod host_call;
mod host_connections;
mod host_exec;
mod import;
mod listing;
mod permission_mode;
mod plugin_call;
mod plugins;
pub(super) mod providers;
mod recording_rpc;
mod rerun_rpc;
mod resume;
mod rewind;
mod scenes;
mod search_rpc;
mod session_import;
mod session_ops;
pub(super) mod settings_reload;
mod settings_write;
mod subscribers;
mod tasks;
mod teardown;
mod trust;
mod turn;
