//! 会话池对外的领域类型：调用方（server、嵌入方）只经由这些类型和池打交道，
//! 不接触 JSON-RPC 线格式。

mod calls;
mod commands;
mod daemon;
mod error;
mod events;
mod history;
mod host_connections;
mod mcp;
mod options;
mod plugin;
mod plugin_call;
mod rerun;
mod rewind;
mod scene;
mod session;
mod settings;
mod tasks;
mod turn;

pub use calls::{
    BlobRef, CallBlob, CallDetail, CallError, CallInput, CallList, CallOutput, CallPart,
    CallRequest, CallResponse, CallStatus, CallSummary, CallTiming, CallUsage, MissingPart,
    RecordingCoverage, RecordingInfo, SearchHit, SearchRequest, SearchResult, SystemBlockSize,
    SystemBlockView, ToolUseRef, ToolUseView,
};
pub use commands::{CommandInfo, CommandKind, CommandSource, CommandTier, NewSchedule};
pub use daemon::{CloseReason, DaemonEvent, EvictReason, ImportSource};
pub use error::{Busy, BusyReason, InvalidOption, PoolError};
pub use events::{
    emitted_outside_turns, AskKind, PendingAsk, SessionEvent, SessionFrame, SessionWatcher,
    Subscribed, WatcherId,
};
pub use history::{
    ArchiveFormat, EntryList, EntryView, ExportSummary, ExportedSession, HistoryItem,
    HistoryOrigin, HistoryPage, HistoryUsage, ImportSummary, ImportedSession, SessionKindName,
};
pub use host_connections::{
    HookAnswer, HooksSet, HostHookOnError, HostHookOption, HostHookSet, HostToolsSet, RefusedEntry,
};
pub use mcp::{
    McpAuthDone, McpAuthStarted, McpAuthState, McpAuthStatus, McpIdentity, McpReconnected,
    McpRedirect, McpServerReconnected, McpServerRef, McpServerState, McpServerStatus,
    McpServerWritten, McpStatusList,
};
pub(crate) use options::requested_permission_mode;
pub use options::{
    ClampedOption, ClockOptions, HostToolOption, HostToolPermit, RecorderOptions, SessionOptions,
    SkillDirOption, TelemetryOptions,
};
pub use plugin::{
    CommandConflict, CommandShadow, PluginCommand, PluginDisclosure, PluginFunctionEntry,
    PluginInfo, PluginInstalled, PluginOrigin, PluginRemoved, PluginScope, PluginToggled,
    SkillConflict, SkillOrigin, VisibleText,
};
pub use plugin_call::{
    PluginCall, PluginCallError, PluginCallOrigin, PluginCallResult, PluginCallTarget,
    PluginFunctionInfo,
};
pub use rerun::{
    ArgDiffView, BlockSelector, RerunDiff, RerunEdits, RerunRequest, RerunResult, StopReasonDiff,
    SystemEdit, TextDiff, ToolStepDiff, ToolStepStatus, ToolUsesDiff, UsageDiff,
};
pub use rewind::{
    CheckpointFile, CheckpointList, CheckpointTurn, InterruptedRewind, RewindCut, RewindOutcome,
    RewindRequest, RewindScope, Rewound,
};
pub use scene::{SceneCapabilities, SceneDescription, SceneReload, SceneSummary, SceneTools};
pub use session::{
    Applies, DeleteReport, ExistingSession, ForkReport, ModelChoice, PermissionModeChoice,
    PermissionReport, PromptChange, PromptChoice, ReasoningChange, Renamed, ResumeReport,
    ResumeStatus, ScriptCall, ScriptCallOutcome, ScriptReport, SessionCreated, SessionDetail,
    ShadowedSkill, SkillDirReport, SkillDirState, TurnState,
};
pub use settings::{
    ConfigTier, ConfigTierName, ExplainTarget, ImportRun, ModelList, ProjectStatus,
    ProjectTrustSet, ProviderConfigView, ProviderPatch, ProviderSaved, ProviderView, RejectKind,
    RoutingStatus, SettingRejected, SettingsChange, SettingsReloaded, SettingsTier,
    SettingsWritten, TrustState,
};
pub use tasks::{AgentStarted, ContextReport, MessageDelivery, SessionTasks, TaskOutput};
pub use turn::{Initiator, TurnOutcome, TurnRequest};
