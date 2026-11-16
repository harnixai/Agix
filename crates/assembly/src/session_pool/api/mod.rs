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
    McpRedirect, McpServerRef, McpServerState, McpServerStatus, McpServerWritten,
};
pub use options::{
    ClockOptions, HostToolOption, RecorderOptions, SessionOptions, SkillDirOption, TelemetryOptions,
};
pub use plugin::{
    PluginDisclosure, PluginInfo, PluginInstalled, PluginRemoved, PluginScope, PluginToggled,
    SkillConflict, SkillOrigin, VisibleText,
};
pub use rewind::{
    CheckpointFile, CheckpointList, CheckpointTurn, InterruptedRewind, RewindCut, RewindOutcome,
    RewindRequest, RewindScope, Rewound,
};
pub use scene::{SceneCapabilities, SceneDescription, SceneReload, SceneSummary, SceneTools};
pub use session::{
    Applies, DeleteReport, ExistingSession, ForkReport, ModelChoice, PermissionReport,
    PromptChoice, ReasoningChange, Renamed, ResumeReport, ResumeStatus, ScriptCall,
    ScriptCallOutcome, ScriptReport, SessionCreated, SessionDetail, ShadowedSkill, SkillDirReport,
    SkillDirState, TurnState,
};
pub use settings::{
    ConfigTier, ConfigTierName, ImportRun, ProjectStatus, ProjectTrustSet, ProviderConfigView,
    ProviderPatch, ProviderSaved, ProviderTier, ProviderView, RoutingStatus, SettingsReloaded,
    TrustState,
};
pub use tasks::{AgentStarted, ContextReport, MessageDelivery, SessionTasks, TaskOutput};
pub use turn::{Initiator, TurnOutcome, TurnRequest};
