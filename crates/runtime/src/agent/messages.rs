//! Wire types of the engine's input, output and permission channels: what hosts send in and what comes back.

use super::*;

#[derive(Debug)]
pub enum InputMessage {
    User {
        content: String,
        attachments: Vec<Attachment>,
        turn_id: String,
    },
    PermissionResponse {
        prompt_id: String,
        decision: PermissionDecision,
    },
    /// The host's answer to an `AgentEvent::QuestionPrompt`.
    QuestionResponse {
        prompt_id: String,
        answer: QuestionAnswer,
    },
    /// How the host call `call_id` (`AgentEvent::HostToolCall`) ended on the
    /// host's side: its answer, or that no host is there to give one.
    HostToolResponse {
        call_id: String,
        end: base::interface::host_call::HostCallEnd,
    },
    System {
        kind: EngineCommand,
        content: String,
    },
    /// A user message meant for the turn that is running, read by the model
    /// at the turn's next step boundary. Only [`Agent::run`] routes these. A
    /// message that finds no turn running starts one, as an ordinary `User`
    /// message would. A sender that needs to know whether a turn took it, and
    /// to keep it when none did, offers it on the session's
    /// [`SteerHandle`](crate::steering::SteerHandle) instead.
    Steer { content: String, message_id: String },
    /// A look at, or a change to, this session's context, handled between
    /// turns. The answer — or why not — goes back on `reply`.
    Context {
        request: ContextRequest,
        reply: tokio::sync::oneshot::Sender<Result<ContextAnswer, ContextError>>,
    },
    /// Write what the session's log does not have yet — the transcript, goal
    /// changes, the durable state — between turns. `reply` says whether the
    /// log now has all of it, or the first write that failed.
    Flush {
        reply: tokio::sync::oneshot::Sender<Result<(), String>>,
    },
    /// Put files, the conversation or both back to where the session was at
    /// an earlier point, between turns (`session.rewind`).
    Rewind {
        request: RewindRequest,
        reply: tokio::sync::oneshot::Sender<Result<RewindReport, RewindError>>,
    },
}

/// What a host asks a rewind to do (`InputMessage::Rewind`).
#[derive(Debug, Clone)]
pub struct RewindRequest {
    pub target: history::checkpoint::RewindTarget,
    pub scope: history::checkpoint::RewindScope,
    /// Overwrite files changed since the session last wrote them.
    pub force: bool,
    /// Leave out the files that cannot be restored, instead of refusing.
    pub allow_partial: bool,
    /// Work out what would happen and change nothing.
    pub dry_run: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RewindIssueKind {
    /// Not what the session last wrote: someone else changed it since.
    Modified,
    /// Its earlier contents were not kept, or no longer match their hash.
    Unavailable,
    /// The write policy refuses it now.
    Denied,
    /// It, or a directory above it, is now a symbolic link.
    Symlink,
    /// It was in a worktree that has since been removed.
    WorktreeRemoved,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
pub struct RewindIssue {
    pub path: String,
    pub issue: RewindIssueKind,
    pub detail: String,
    /// `force` or `allow_partial`.
    pub overridable_by: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
pub struct RestoredFile {
    pub path: String,
    /// `file` or `absent`.
    pub to: String,
    pub sha256: Option<String>,
    pub outside_project: bool,
    /// The session's last write of it has no record of what it left, so it
    /// was not checked for changes made since.
    pub unverified: bool,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, schemars::JsonSchema)]
pub struct RemovedTurn {
    pub turn_id: String,
    /// What the turn was started with, for a host to offer again.
    pub prompt: Vec<base::message::ContentBlock>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, schemars::JsonSchema)]
pub struct RewoundConversation {
    pub messages_before: usize,
    pub messages_after: usize,
    pub removed_turns: Vec<RemovedTurn>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
pub struct RewoundFiles {
    pub restored: Vec<RestoredFile>,
    pub skipped: Vec<RewindIssue>,
}

/// What a rewind did, or with `dry_run` would do.
#[derive(Debug, Clone, PartialEq, serde::Serialize, schemars::JsonSchema)]
pub struct RewindReport {
    pub rewind_id: String,
    pub dry_run: bool,
    /// `None` when the conversation was not part of it.
    pub conversation: Option<RewoundConversation>,
    /// `None` when the files were not part of it.
    pub files: Option<RewoundFiles>,
}

/// Why a rewind changed nothing, or not everything.
#[derive(Debug, Clone, PartialEq)]
pub enum RewindError {
    /// The request does not fit this session.
    Invalid(String),
    /// Files that cannot be restored as asked; nothing was changed.
    Precondition(Vec<RewindIssue>),
    Failed(RewindFailure),
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
pub struct RewindFailure {
    pub rewind_id: String,
    pub failed: FailedPath,
    /// Restored, then put back as they were before the rewind.
    pub rolled_back: Vec<String>,
    /// Restored and could not be put back.
    pub not_rolled_back: Vec<String>,
    /// Some files are left changed: `not_rolled_back` is not empty.
    pub partial: bool,
    pub commit_not_recorded: bool,
    pub conversation_changed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
pub struct FailedPath {
    pub path: String,
    pub error: String,
}

impl std::fmt::Display for RewindError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(why) => f.write_str(why),
            Self::Precondition(issues) => write!(
                f,
                "{} file(s) cannot be restored as asked; nothing was changed",
                issues.len()
            ),
            Self::Failed(failure) => write!(
                f,
                "the rewind failed at {}: {}",
                failure.failed.path, failure.failed.error
            ),
        }
    }
}

/// Why a [`ContextRequest`] was not answered as asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContextError {
    /// The request does not fit the context as it is; nothing changed.
    Refused(String),
    /// The change was made, but the session's log does not have it, so a
    /// resume will not reproduce it.
    NotRecorded(String),
}

impl std::fmt::Display for ContextError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refused(why) => f.write_str(why),
            Self::NotRecorded(why) => write!(f, "the change was made but not logged: {why}"),
        }
    }
}

/// What a host asks of a session's context (`InputMessage::Context`).
///
/// Positions are those of the session's messages as `session.history`
/// numbers them. `total`, when given, is how many messages the asker saw: a
/// change refuses to apply if the context has since grown or been compacted,
/// rather than land on a message the asker did not mean.
#[derive(Debug, Clone)]
pub enum ContextRequest {
    /// What the next model call would carry, without making it.
    Inspect,
    /// Blank out user messages or tool results (see `Transcript::drop_message`).
    Drop {
        at: Vec<usize>,
        total: Option<usize>,
    },
    /// Rewrite a message the user typed.
    Edit {
        at: usize,
        text: String,
        total: Option<usize>,
    },
}

/// The engine's answer to a [`ContextRequest`], variant for variant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContextAnswer {
    Inspected(ContextReport),
    Dropped(Vec<usize>),
    Edited(usize),
}

/// What the next model call would carry if a step started now with no new
/// input, and roughly what each part costs.
#[derive(
    Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
pub struct ContextReport {
    /// Always `next_step_without_new_input`.
    pub basis: String,
    pub model: String,
    pub max_tokens: u32,
    pub system: Vec<SystemBlockReport>,
    pub tools: Vec<ToolReport>,
    pub messages: MessagesReport,
    /// A local estimate.
    pub total_tokens: usize,
    pub compact_threshold: usize,
    pub hard_cap: Option<usize>,
}

#[derive(
    Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
pub struct SystemBlockReport {
    pub name: Option<String>,
    pub role: base::prompt::BlockRole,
    pub origin: base::prompt::BlockOrigin,
    pub tokens: usize,
    pub text: String,
}

#[derive(
    Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
pub struct ToolReport {
    pub name: String,
    pub source: Option<String>,
    pub schema_bytes: usize,
    pub tokens: usize,
}

#[derive(
    Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
pub struct MessagesReport {
    pub count: usize,
    pub tokens: usize,
}

/// Something the user attached to their message — a pasted screenshot, a
/// dragged-in file, an editor selection.
///
/// The variants below are the three shapes hosts actually have, so that a host
/// can hand over whichever it already holds instead of being forced to pre-read
/// (or pre-decode) on the runtime's behalf.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Attachment {
    /// A path the runtime should read. Images (by extension) become
    /// [`ModelContentBlock::Image`], `.pdf` into [`ModelContentBlock::Document`];
    /// anything else is inlined as text.
    /// Use this when the host only has a path — e.g. a drag-and-drop.
    File { path: String },
    /// Text the host has already read. `path` is kept for the label shown to
    /// the model, and may be a synthetic name for non-file text such as an
    /// editor selection.
    Text { path: String, content: String },
    /// An image the host already decoded — the clipboard-paste case, where
    /// there is no file on disk at all. `data` is base64 without a data-URI
    /// prefix.
    Image { media_type: String, data: String },
    /// A PDF the host already encoded. `data` is base64 without a data-URI
    /// prefix; `name` is the label the model and the provider see.
    Document {
        media_type: String,
        data: String,
        #[serde(default)]
        name: Option<String>,
    },
}

/// Where a `PermitAlways` decision's effect should be retained.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PersistScope {
    /// In-memory only, this session.
    Session,
    /// In-memory for this session AND written to `.agix/settings.local.json`
    /// so future sessions in this project also allow it.
    Local,
}

/// Wire shape: `{"type":"permit"}` / `{"type":"deny","reason":"..."}` /
/// `{"type":"permit_always","scope":"session"}` /
/// `{"type":"permit_always","scope":"local"}` — what
/// `session.respondToPrompt` (daemon RPC) parses out of its `decision`
/// param and what `InputMessage::PermissionResponse` carries.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PermissionDecision {
    Permit,
    Deny {
        reason: String,
    },
    /// Like `Permit`, but also persists an Allow rule beyond this single
    /// tool call — see `base::interface::permission::PermissionGrants`.
    /// `scope: Session` keeps it in-memory only (rest of this session);
    /// `scope: Local` additionally writes it to `.agix/settings.local.json`
    /// so future sessions in this project inherit it too.
    PermitAlways {
        scope: PersistScope,
    },
}

/// Wire shape: `{"type":"answer","text":"..."}` / `{"type":"decline","reason":"..."}`
/// — what `session.respondToPrompt` parses out of its `answer` param for a
/// question and what `InputMessage::QuestionResponse` carries. `text` is one
/// of the question's option keys or free text.
#[derive(
    Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum QuestionAnswer {
    Answer { text: String },
    Decline { reason: String },
}

#[derive(Debug, Clone)]
pub enum EngineCommand {
    SetSessionId,
    CompactNow,
    RefreshMcp,
    /// Interrupt the turn in flight and keep the session alive — what a host
    /// binds to Esc. Distinct from cancelling the token passed to
    /// [`Agent::run`], which ends the whole session.
    ///
    /// Handled in the input demultiplexer, never in the main loop: the loop
    /// awaits `process_turn` inline, so while a turn is running it is not
    /// reading the channel and could only act on this once the turn it was
    /// meant to interrupt had already finished. Same reason
    /// `PermissionResponse` is handled there.
    ///
    /// `turn_id` names the turn to cancel; one whose message has not been
    /// queued yet starts cancelled when it is. `None` is the turn last
    /// queued. Either way a turn that has already ended is left alone: its
    /// background tasks were meant to outlive it.
    CancelTurn {
        turn_id: Option<String>,
    },
    Shutdown,
}

/// Why the host is stopping an engine: what its `SessionEnd` hooks get as
/// `reason`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndReason {
    /// `session.close` or `session.delete`.
    Close,
    /// Idle, or pushed out of a full pool.
    Evict,
    /// Replaced by an engine built anew for the same session.
    Rebuild,
    /// The host itself is shutting down.
    Shutdown,
}

impl EndReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Close => "close",
            Self::Evict => "evict",
            Self::Rebuild => "rebuild",
            Self::Shutdown => "shutdown",
        }
    }
}

/// Why the host starts an engine: what its `SessionStart` hooks get as
/// `source`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartSource {
    /// A session with nothing before it.
    New,
    /// A session brought back from its log, after a restart or an eviction.
    Resume,
    /// A new engine for a session whose engine was replaced.
    Rebuild,
}

impl StartSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::New => "new",
            Self::Resume => "resume",
            Self::Rebuild => "rebuild",
        }
    }
}

/// Where a host says why it stops an engine, before it cancels the token
/// [`Agent::run`] was handed ([`Agent::end_reason`]). The first reason set
/// stands. An engine stopped without one, or that ends on its own — its input
/// channel closed, [`EngineCommand::Shutdown`] — fires `SessionEnd` with no
/// `reason`.
#[derive(Debug, Clone, Default)]
pub struct EndReasonSlot(Arc<std::sync::OnceLock<EndReason>>);

impl EndReasonSlot {
    pub fn set(&self, reason: EndReason) {
        let _ = self.0.set(reason);
    }

    pub fn get(&self) -> Option<EndReason> {
        self.0.get().copied()
    }
}

pub type InputSender = mpsc::UnboundedSender<InputMessage>;
pub type InputReceiver = mpsc::UnboundedReceiver<InputMessage>;
/// Where the engine emits. Every emission site in the runtime holds one of
/// these, which is why swapping the raw channel for [`EventBus`] reached all
/// of them at once — the bus keeps `send`'s signature deliberately.
///
/// [`EventBus`]: crate::event_bus::EventBus
pub type EventSender = crate::event_bus::EventBus;
pub type EventReceiver = mpsc::UnboundedReceiver<AgentEvent>;

/// What [`Builder::session_prompt`] puts over a session's settings. `None`
/// leaves the settings' `prompt_append` / `prompt_override` in force.
#[derive(
    Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
pub struct SessionPrompt {
    pub append: Option<String>,
    #[serde(rename = "override")]
    pub override_text: Option<String>,
}

impl SessionPrompt {
    pub fn is_empty(&self) -> bool {
        self.append.is_none() && self.override_text.is_none()
    }

    pub(super) fn applied_to(&self, settings: &LoadedSettings) -> LoadedSettings {
        let mut out = settings.clone();
        if let Some(append) = &self.append {
            out.values.prompt_append = Some(append.clone());
        }
        if let Some(text) = &self.override_text {
            out.values.prompt_override = Some(text.clone());
        }
        out
    }
}

/// Permission questions awaiting the host's answer, keyed by `prompt_id`. A
/// sub-agent's questions are registered in its parent's (`agents`'
/// `ParentElicitation`), so the parent's host answers them as its own.
pub type PendingPermissions = Arc<
    std::sync::Mutex<
        std::collections::HashMap<String, tokio::sync::oneshot::Sender<PermissionDecision>>,
    >,
>;
