//! Session persistence: the [`HistoryStore`] contract and the implementations
//! that ship with the engine — JSONL under a project directory, in memory,
//! and a decorator that lets observers see appends.
//!
//! A `JsonlHistoryStore` serves **many projects**: each session's transcript
//! goes to the project it was created under (`Meta.project_root`), and the
//! cwd the store was built with is only the default for a session that never
//! said. Many sessions share one store, and writes within a session are
//! serialized by a mutex so two appends cannot interleave into a partial
//! line. Across processes a session's log has one writer, held by a file lock
//! (`crate::lock`). A last line cut short by a crash is left out when
//! the log is read and cut off before the next append.

mod externalize;
mod in_memory;
mod jsonl;
mod observed;

use jsonl::files::{format_ts, lineage_of, unknown_time};

pub use in_memory::InMemoryHistoryStore;
pub use jsonl::JsonlHistoryStore;
pub use observed::{AppendObserver, ObservedHistoryStore};

use crate::entry::{EnvelopedEntry, LogEntry, SessionKind};
use crate::error::HistoryError;
use crate::query::SessionQuery;
use crate::transcript::{
    messages_match_query, preview_messages, project_messages_with, DefaultProjection,
    TranscriptProjection,
};
use async_trait::async_trait;
use base::session::SessionId;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSummary {
    pub session_id: SessionId,
    pub last_modified: String,
    pub entry_count: usize,
    pub message_count: usize,
    pub preview: String,
    pub canonical_cwd: Option<String>,
    pub title: Option<String>,
    pub total_input_tokens: Option<u64>,
    pub total_output_tokens: Option<u64>,
    pub compact_count: u64,
}

/// What a session is: the facts fixed when it was created.
///
/// Distinct from [`SessionSummary`], which is about the conversation and
/// changes with every turn. These come from the log's `Meta` entry and never
/// change, which is why a listing can carry them and why nothing needs to
/// keep a second copy in step.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionFacts {
    /// Scene the session was created under. `None` on pre-v2 logs.
    pub scene: Option<String>,
    /// Absolute project root, or `None` for a no-project session — and also
    /// `None` on pre-v2 logs, which did not record one. The two cases are not
    /// distinguished here because nothing acts differently on them.
    pub project_root: Option<String>,
    pub session_kind: SessionKind,
    pub parent_session_id: Option<String>,
}

impl SessionFacts {
    /// The facts a `Meta` entry carries, or `None` for any other entry.
    pub fn of(entry: &LogEntry) -> Option<Self> {
        match entry {
            LogEntry::Meta {
                scene,
                project_root,
                session_kind,
                parent_session_id,
                ..
            } => Some(Self {
                scene: scene.clone(),
                project_root: project_root.clone(),
                session_kind: *session_kind,
                parent_session_id: parent_session_id.clone(),
            }),
            _ => None,
        }
    }
}

/// What has been recorded about a session rather than in it.
///
/// Counted as a by-product of appends that already happened — the alternative
/// is reading a whole transcript per session to answer "how much did this
/// cost", per listing, forever.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionStats {
    /// The name somebody chose, if anybody did.
    pub title: Option<String>,
    /// How many times this session's context has been compacted.
    pub compact_count: u64,
}

/// A session and the sidechains under it, read as one piece.
///
/// `sessions` starts with the root and continues breadth-first through every
/// sidechain at any depth, each with the entries its log held when the
/// snapshot was taken. A child recorded as a fork (`session_kind: primary`)
/// belongs to nobody's lineage but its own: it is named in `forks` and not
/// walked into.
#[derive(Debug, Clone, Default)]
pub struct LineageSnapshot {
    pub sessions: Vec<(SessionId, Vec<EnvelopedEntry>)>,
    pub forks: Vec<SessionId>,
}

/// A hold on one session's log, from [`HistoryStore::claim`]; released when
/// dropped.
#[derive(Default)]
pub struct SessionClaim(Option<Box<dyn std::any::Any + Send + Sync>>);

impl SessionClaim {
    /// A claim kept by `guard`, for a backend's own implementation.
    pub fn new(guard: impl std::any::Any + Send + Sync) -> Self {
        Self(Some(Box::new(guard)))
    }
}

impl std::fmt::Debug for SessionClaim {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("SessionClaim")
            .field(&self.0.is_some())
            .finish()
    }
}

/// Where a session's log lives between runs.
///
/// The engine writes an append-only sequence of [`LogEntry`] per session and
/// reads it back to resume, fork or list. Nothing above this trait knows how
/// or where that happens: a backend may keep it in files, in a database, or
/// in memory, and the two shipped implementations do two of those.
///
/// # What an implementation must guarantee
///
/// * **Append-only order.** [`load`](Self::load) returns entries in the order
///   [`append`](Self::append) received them. Resume, fork and replay all read
///   position as meaning, so a store that reorders is not a slower store, it
///   is a wrong one.
/// * **A missing session is [`HistoryError::SessionNotFound`]**, not an empty
///   log. "Never existed" and "exists and is empty" are different answers and
///   callers act on the difference.
/// * **Durability is the backend's promise, not the caller's.** `append`
///   returning `Ok` means the entry is in the log; [`sync`](Self::sync)
///   returning `Ok` means everything appended before it survives whatever
///   that backend survives.
#[async_trait]
pub trait HistoryStore: Send + Sync {
    async fn append(&self, session: SessionId, entry: LogEntry) -> Result<(), HistoryError>;

    /// [`append`](Self::append), recording what the writer knows about the
    /// line in its envelope. A backend that cannot keep the tags may drop
    /// them; the line itself must still be written.
    async fn append_tagged(
        &self,
        session: SessionId,
        entry: LogEntry,
        tags: crate::entry::EntryTags,
    ) -> Result<(), HistoryError> {
        let _ = tags;
        self.append(session, entry).await
    }

    /// Make everything appended to `session` so far durable.
    ///
    /// Separate from `append` because durability costs a disk flush: the
    /// engine appends entry by entry and syncs once per step. The default does
    /// nothing, which is right for a backend that is durable on append or not
    /// durable at all.
    async fn sync(&self, _session: SessionId) -> Result<(), HistoryError> {
        Ok(())
    }

    /// Keep other processes from writing `session` until the claim is
    /// dropped — [`HistoryError::Locked`] when one already is. Appends take
    /// the same hold for as long as they run, so a claim is for keeping it
    /// between them: a session open in this process is held by it. The
    /// default claims nothing, for a backend no other process can reach.
    async fn claim(&self, _session: SessionId) -> Result<SessionClaim, HistoryError> {
        Ok(SessionClaim::default())
    }

    async fn load(&self, session: SessionId) -> Result<Vec<EnvelopedEntry>, HistoryError>;
    async fn list_sessions(&self) -> Result<Vec<SessionId>, HistoryError>;

    /// Forget a session entirely — its log and anything the backend keeps
    /// alongside it. Deleting a session that is not there is not an error.
    async fn delete(&self, session: SessionId) -> Result<(), HistoryError>;

    /// The session's log as messages the model can be given directly.
    ///
    /// Projection rules, which are the engine's and not a backend's:
    /// - User / Assistant map across one to one;
    /// - a run of ToolResult entries collapses into a single `Message::User`,
    ///   because the API wants one user message carrying every `tool_result`
    ///   block of a round;
    /// - Meta / System / Compact / UsageSnapshot never reach the model.
    async fn load_messages(
        &self,
        session: SessionId,
    ) -> Result<Vec<base::message::Message>, HistoryError> {
        let entries = self.load(session).await?;
        Ok(project_messages_with(&entries, self.projection()))
    }

    /// Where this store's kept time and kept identifiers come from.
    ///
    /// On the store for the same reason the projection is: an entry's id and
    /// timestamp are written here and nowhere else, so this is the only place
    /// that has to be told.
    fn environment(&self) -> &dyn base::interface::environment::Environment {
        &base::interface::environment::SystemEnvironment
    }

    /// The rules this store's logs are read under.
    ///
    /// On the store rather than the engine because a log and the rules for
    /// reading it travel together: forking, resuming and searching a
    /// transcript must all see the same conversation, and they reach it
    /// through here.
    fn projection(&self) -> &dyn TranscriptProjection {
        &DefaultProjection
    }

    /// What a session is, as opposed to what was said in it.
    ///
    /// Scene, project and lineage are decided when a session is created and
    /// never change, and they are all written to the log's `Meta` entry — the
    /// first line of the file. A backend that can read that line alone
    /// answers this without materializing a transcript, which is what makes
    /// it usable from a listing: overriding this one method is enough to make
    /// both the parent-child walk and `session.list`'s per-entry facts cheap.
    ///
    /// Every field is optional in the same sense: a pre-v2 log has no
    /// `scene`/`project_root`, and the honest answer is `None` rather than a
    /// guess. Callers that need a scene for a *resume* infer it from the
    /// request instead (docs/daemon_rpc_protocol.md §3.4).
    async fn session_facts(&self, session: SessionId) -> Result<SessionFacts, HistoryError> {
        let entries = match self.load(session).await {
            Ok(e) => e,
            Err(HistoryError::SessionNotFound(_)) => return Ok(SessionFacts::default()),
            Err(e) => return Err(e),
        };
        Ok(entries
            .iter()
            .find_map(|env| SessionFacts::of(&env.entry))
            .unwrap_or_default())
    }

    /// Give this session the name its owner calls it, or clear it (`None`).
    ///
    /// Not an entry in the log, and deliberately so. A name is not something
    /// that happened in the conversation — it is chosen afterwards, changed
    /// afterwards, and belongs to whoever is looking at the session rather
    /// than to the session's own history. It also has to be readable without
    /// replaying anything, which is why it lives beside the log instead of
    /// in it.
    ///
    /// The default refuses: a backend that cannot keep a name should say so
    /// rather than accept one and forget it, which is the failure a host
    /// discovers only when the name is gone.
    async fn set_session_title(
        &self,
        _session: SessionId,
        _title: Option<String>,
    ) -> Result<(), HistoryError> {
        Err(HistoryError::Path(
            "this history store cannot name sessions".into(),
        ))
    }

    /// What has been recorded *about* one session, as opposed to in it.
    ///
    /// The name somebody gave it and the running totals, which is everything
    /// that cannot be answered from the log's first line and should not cost
    /// a read of the whole log. A backend that keeps none of this answers
    /// with the empty record, and every field of it is honestly "nothing
    /// counted" rather than zero.
    async fn session_stats(&self, _session: SessionId) -> Result<SessionStats, HistoryError> {
        Ok(SessionStats::default())
    }

    /// Which session spawned this one, if any.
    ///
    /// Reads through [`session_facts`](Self::session_facts) so a backend has
    /// one place to make head-of-file lookups cheap rather than two that can
    /// disagree about the same line.
    async fn session_parent(&self, session: SessionId) -> Result<Option<String>, HistoryError> {
        Ok(self.session_facts(session).await?.parent_session_id)
    }

    /// Every session this one spawned.
    ///
    /// The default asks each known session who its parent is, which is one
    /// [`session_parent`](Self::session_parent) call per session and no better
    /// than linear. A backend that indexes the parent should override this
    /// outright; one that can merely read a session's head cheaply gets most
    /// of the win from overriding `session_parent` instead.
    async fn child_sessions(
        &self,
        parent_session_id: &str,
    ) -> Result<Vec<SessionId>, HistoryError> {
        let all = self.list_sessions().await?;
        let mut out = Vec::new();
        for sid in all {
            if self.session_parent(sid).await?.as_deref() == Some(parent_session_id) {
                out.push(sid);
            }
        }
        Ok(out)
    }

    /// `root` and every sidechain under it — see [`LineageSnapshot`].
    ///
    /// An entry appended after the snapshot is taken is in none of its logs,
    /// and a child created after it is not among its sessions. The default
    /// reads one session after another, so its logs are not cut at a single
    /// instant: a sidechain still being written may come back with entries
    /// appended after its parent was read. A backend that can hold its
    /// writers off for the moment it takes to measure every log should
    /// override this.
    ///
    /// A missing root is [`HistoryError::SessionNotFound`]; a sidechain that
    /// disappears while it is being read is left out.
    async fn snapshot_lineage(&self, root: SessionId) -> Result<LineageSnapshot, HistoryError> {
        let mut snapshot = LineageSnapshot {
            sessions: vec![(root, self.load(root).await?)],
            forks: Vec::new(),
        };
        let mut seen = std::collections::HashSet::from([root]);
        let mut next = 0;
        while next < snapshot.sessions.len() {
            let parent = snapshot.sessions[next].0.to_string();
            next += 1;
            let mut children = self.child_sessions(&parent).await?;
            children.sort_by_key(SessionId::to_string);
            for child in children {
                if !seen.insert(child) {
                    continue;
                }
                if self.session_facts(child).await?.session_kind != SessionKind::Sidechain {
                    snapshot.forks.push(child);
                    continue;
                }
                match self.load(child).await {
                    Ok(entries) => snapshot.sessions.push((child, entries)),
                    Err(HistoryError::SessionNotFound(_)) => {}
                    Err(e) => return Err(e),
                }
            }
        }
        Ok(snapshot)
    }

    /// Sessions matching a [`SessionQuery`], newest first.
    ///
    /// This is the whole of "which sessions are there" — recency listing and
    /// text search are the same question with and without a needle, so a
    /// backend that can answer one cheaply can answer both.
    ///
    /// # What an implementation must guarantee
    ///
    /// * **Newest first**, and a total order: ties on modification time break
    ///   on the session id, descending. Two backends holding the same sessions
    ///   answer in the same order or one of them is wrong.
    /// * **At most `limit`**, and `limit == 0` is the empty answer rather than
    ///   an unbounded one.
    /// * **A session whose transcript contains the needle is in the answer**
    ///   if the limit allows, matched case-insensitively over the rendered
    ///   text; so is one whose id contains it. A backend may match *more* than
    ///   that — an index over titles or working directories is why this is a
    ///   contract and not a function — but never less.
    ///
    /// The default reads every session to answer, which is exactly what a
    /// backend with an index should not do: overriding this one method is the
    /// whole of taking search over, and nothing above the trait changes.
    async fn find_sessions(
        &self,
        query: &SessionQuery,
    ) -> Result<Vec<SessionSummary>, HistoryError> {
        if query.limit == 0 {
            return Ok(Vec::new());
        }
        let needle = query.needle();
        let mut rows: Vec<(Option<time::OffsetDateTime>, SessionSummary)> = Vec::new();
        for session_id in self.list_sessions().await? {
            let entries = match self.load(session_id).await {
                Ok(entries) => entries,
                Err(HistoryError::SessionNotFound(_)) => continue,
                Err(e) => return Err(e),
            };
            let messages = project_messages_with(&entries, self.projection());
            if let Some(needle) = needle {
                if !session_id.to_string().contains(needle)
                    && !messages_match_query(&messages, needle)
                {
                    continue;
                }
            }
            let modified = entries.iter().map(|env| env.ts).max();
            rows.push((
                modified,
                SessionSummary {
                    session_id,
                    last_modified: modified.map(format_ts).unwrap_or_else(unknown_time),
                    entry_count: entries.len(),
                    message_count: messages.len(),
                    preview: preview_messages(&messages, 140),
                    canonical_cwd: None,
                    title: None,
                    total_input_tokens: None,
                    total_output_tokens: None,
                    compact_count: 0,
                },
            ));
        }
        rows.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then_with(|| b.1.session_id.to_string().cmp(&a.1.session_id.to_string()))
        });
        rows.truncate(query.limit);
        Ok(rows.into_iter().map(|(_, summary)| summary).collect())
    }
}

#[cfg(test)]
mod tests;
