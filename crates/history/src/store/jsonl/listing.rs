//! Finding sessions across where they are kept: a project's are those in its
//! global transcript directory and those its history directory holds under
//! an index; every project's are all of those plus every indexed session.

use super::files::*;
use super::homes::{same_place, Access, Home};
use super::JsonlHistoryStore;
use crate::error::HistoryError;
use crate::path::project_dir_for;
use crate::project::SessionMetadata;
use crate::store::{HistoryStore, SessionSummary};
use crate::transcript::{messages_match_query, preview_messages, project_messages_with};
use base::session::SessionId;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// One transcript found by a listing, and the directory its sidecar sits in.
pub(super) struct Found {
    pub(super) session: SessionId,
    pub(super) path: PathBuf,
    pub(super) mtime: SystemTime,
    pub(super) sidecars: PathBuf,
}

fn modified(path: &Path) -> SystemTime {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .unwrap_or(std::time::UNIX_EPOCH)
}

impl JsonlHistoryStore {
    fn found_kept(&self, kept: Vec<(SessionId, Home)>) -> Vec<Found> {
        kept.into_iter()
            .map(|(session, home)| {
                let path = self.homes.transcript(&home, session);
                Found {
                    session,
                    mtime: modified(&path),
                    path,
                    sidecars: self.homes.sidecar_root(&home).to_path_buf(),
                }
            })
            .filter(|found| found.path.is_file())
            .collect()
    }

    fn found_global(&self, files: Vec<(SessionId, PathBuf, SystemTime)>) -> Vec<Found> {
        files
            .into_iter()
            .map(|(session, path, mtime)| Found {
                session,
                path,
                mtime,
                sidecars: self.homes.sessions_root.clone(),
            })
            .collect()
    }

    /// The sessions of the project at `root` (`None`, the no-project tier),
    /// newest first.
    pub(super) async fn sessions_of(
        &self,
        root: Option<&Path>,
    ) -> Result<Vec<Found>, HistoryError> {
        let root = match root {
            Some(root) => Some(
                crate::path::canonicalize_cwd(root)
                    .await
                    .unwrap_or_else(|_| root.to_path_buf()),
            ),
            None => None,
        };
        let dir = project_dir_for(&self.homes.projects_root, root.as_deref());
        let mut found = self.found_global(collect_session_files(dir).await?);
        if let Some(root) = root {
            self.homes.note_project(&root);
            let homes = self.homes.clone();
            let kept = blocking(move || {
                let Some(history) = homes.projects().and_then(|p| p.history_of(&root)) else {
                    return Ok(Vec::new());
                };
                Ok(homes
                    .indexed_in(&history.dir, Access::Read)
                    .into_iter()
                    .filter(|(_, home)| {
                        home.project
                            .as_deref()
                            .is_some_and(|p| same_place(p, &root))
                    })
                    .collect())
            })
            .await?;
            found.extend(self.found_kept(kept));
        }
        found.sort_by_key(|f| std::cmp::Reverse(f.mtime));
        Ok(found)
    }

    /// Every session this store can reach, newest first.
    pub(super) async fn every_session(&self) -> Result<Vec<Found>, HistoryError> {
        let mut found = self.found_global(collect_every_project(&self.homes.projects_root).await?);
        let homes = self.homes.clone();
        let kept = blocking(move || Ok(homes.all_indexed())).await?;
        found.extend(self.found_kept(kept));
        found.sort_by_key(|f| std::cmp::Reverse(f.mtime));
        Ok(found)
    }

    pub(super) async fn recent_summaries(
        &self,
        found: Vec<Found>,
        max: usize,
        detail: crate::query::SummaryDetail,
    ) -> Result<Vec<SessionSummary>, HistoryError> {
        let mut out = Vec::new();
        for found in found.into_iter().take(max) {
            match detail {
                // The listing already answered the question. Opening each
                // transcript to fill in a preview nobody asked for turns a
                // listing into a full parse of every session in the project.
                //
                // The sidecar is a different matter and is read: it is one
                // small JSON document per session, and it holds the things
                // that exist nowhere else — the name a person gave this
                // session, and the counters. Leaving it out would bring a
                // renamed session back unnamed.
                crate::query::SummaryDetail::IdsOnly => {
                    let metadata = load_session_metadata(&found.sidecars, found.session).await;
                    out.push(summary_from_parts(
                        found.session,
                        found.mtime,
                        0,
                        &[],
                        metadata,
                    ))
                }
                crate::query::SummaryDetail::Full => {
                    if let Some(summary) = self.session_summary(&found).await? {
                        out.push(summary);
                    }
                }
            }
        }
        Ok(out)
    }

    pub(super) async fn search_sessions(
        &self,
        found: Vec<Found>,
        needle: &str,
        max: usize,
    ) -> Result<Vec<SessionSummary>, HistoryError> {
        let mut out = Vec::new();
        for found in found {
            let entries = match self.load(found.session).await {
                Ok(entries) => entries,
                Err(HistoryError::SessionNotFound(_)) => continue,
                Err(e) => {
                    skip_unreadable(found.session, &e);
                    continue;
                }
            };
            let messages = project_messages_with(&entries, self.projection());
            if found.session.to_string().contains(needle) || messages_match_query(&messages, needle)
            {
                let metadata = load_session_metadata(&found.sidecars, found.session).await;
                out.push(summary_from_parts(
                    found.session,
                    found.mtime,
                    entries.len(),
                    &messages,
                    metadata,
                ));
            }
            if out.len() >= max {
                break;
            }
        }
        Ok(out)
    }

    pub(super) async fn search_everywhere(
        &self,
        query: &str,
        max: usize,
        filter: ProjectDirFilter,
    ) -> Result<Vec<SessionSummary>, HistoryError> {
        if max == 0 {
            return Ok(Vec::new());
        }
        let query = query.trim().to_lowercase();
        let mut out = Vec::new();
        for found in self.every_session().await? {
            let metadata = load_session_metadata(&found.sidecars, found.session).await;
            if !filter.matches(metadata.as_ref()) {
                continue;
            }
            let entries = match read_log(&found.path).await {
                Ok(Some(content)) => parse_log(&content, self.blobs.as_deref()),
                Ok(None) => continue,
                Err(e) => Err(e),
            };
            let entries = match entries {
                Ok(entries) => entries,
                Err(e) => {
                    skip_unreadable(found.session, &e);
                    continue;
                }
            };
            let messages = project_messages_with(&entries, self.projection());
            let haystack = format!(
                "{}\n{}\n{}\n{}",
                found.session,
                metadata
                    .as_ref()
                    .and_then(|m| m.title.as_deref())
                    .unwrap_or_default(),
                metadata
                    .as_ref()
                    .map(|m| m.canonical_cwd.as_str())
                    .unwrap_or_default(),
                crate::transcript::render_search_text(&messages)
            )
            .to_lowercase();
            if query.is_empty() || haystack.contains(&query) {
                out.push(summary_from_parts(
                    found.session,
                    found.mtime,
                    entries.len(),
                    &messages,
                    metadata,
                ));
            }
            if out.len() >= max {
                break;
            }
        }
        Ok(out)
    }

    /// `None` for a session whose log cannot be read back. A half-written or
    /// hand-edited transcript is a property of that one session; letting it
    /// fail the call would hide every other session in the directory behind
    /// it.
    async fn session_summary(&self, found: &Found) -> Result<Option<SessionSummary>, HistoryError> {
        let entries = match self.load(found.session).await {
            Ok(entries) => entries,
            Err(HistoryError::SessionNotFound(_))
            | Err(HistoryError::Parse { .. })
            | Err(HistoryError::ProjectUntrusted { .. }) => return Ok(None),
            Err(e) => return Err(e),
        };
        let messages = project_messages_with(&entries, self.projection());
        let metadata = load_session_metadata(&found.sidecars, found.session).await;
        Ok(Some(summary_from_parts(
            found.session,
            found.mtime,
            entries.len(),
            &messages,
            metadata,
        )))
    }
}

fn summary_from_parts(
    session_id: SessionId,
    mtime: SystemTime,
    entry_count: usize,
    messages: &[base::message::Message],
    metadata: Option<SessionMetadata>,
) -> SessionSummary {
    SessionSummary {
        session_id,
        last_modified: format_mtime(mtime),
        entry_count,
        message_count: messages.len(),
        preview: metadata
            .as_ref()
            .and_then(|m| m.latest_summary.clone())
            .unwrap_or_else(|| preview_messages(messages, 140)),
        canonical_cwd: metadata.as_ref().map(|m| m.canonical_cwd.clone()),
        title: metadata.as_ref().and_then(|m| m.title.clone()),
        total_input_tokens: metadata.as_ref().and_then(|m| m.total_input_tokens),
        total_output_tokens: metadata.as_ref().and_then(|m| m.total_output_tokens),
        compact_count: metadata.as_ref().map(|m| m.compact_count).unwrap_or(0),
    }
}
