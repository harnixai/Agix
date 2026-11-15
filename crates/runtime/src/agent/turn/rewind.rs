//! 宿主的回滚（`InputMessage::Rewind`）在引擎里的执行：算切点与计划、预检、`begin`、逐个恢复
//! 文件（每次恢复本身也是一次被快照的写）、失败时把已恢复的退回并写 `abort`、成功写 `commit`，
//! 然后从日志重载会话——回滚之后的会话必须等于把它从日志 resume 出来的会话。
//!
//! 结果只有三种：全做了、什么都没变、部分（只在退回也失败时），第三种如实报出每个路径。

use crate::agent::{
    Agent, FailedPath, RemovedTurn, RestoredFile, RewindError, RewindFailure, RewindIssue,
    RewindIssueKind, RewindReport, RewindRequest, RewoundConversation, RewoundFiles,
};
use crate::checkpoint::{Checkpoints, WriteBy};
use base::interface::exec::FileSystem;
use history::checkpoint::{plan, After, Before, RewindTarget, REWIND_NS};
use history::entry::{EnvelopedEntry, LogEntry};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// One file the rewind will write, once the preflight let it through.
struct Restore {
    shown: String,
    target: PathBuf,
    to: Option<Vec<u8>>,
    outside_project: bool,
    unverified: bool,
}

/// A file as it was just before this rewind restored it.
struct Undo {
    shown: String,
    target: PathBuf,
    prior: Option<Vec<u8>>,
}

struct Plan {
    entries: Vec<EnvelopedEntry>,
    keep: usize,
    restores: Vec<Restore>,
    issues: Vec<RewindIssue>,
    overwrote_external: Vec<String>,
}

impl Agent {
    pub(super) async fn rewind(
        &mut self,
        request: RewindRequest,
    ) -> Result<RewindReport, RewindError> {
        let plan = self.plan_rewind(&request).await?;
        let blocking: Vec<RewindIssue> = plan
            .issues
            .iter()
            .filter(|i| !overridden(i, &request))
            .cloned()
            .collect();
        let rewind_id = self.policies.environment().new_id().to_string();
        if request.dry_run {
            let mut report = self.rewind_report(&request, &plan, &rewind_id, true);
            if let Some(files) = report.files.as_mut() {
                files.skipped = plan.issues.clone();
            }
            return Ok(report);
        }
        if !blocking.is_empty() {
            return Err(RewindError::Precondition(blocking));
        }
        let before = self.session.messages().len();
        self.record_rewind(
            history::checkpoint::BEGIN,
            &begin_payload(&request, &plan, &rewind_id),
        )
        .await
        .map_err(|e| self.failed_before_any(&rewind_id, e))?;
        let undo = self.restore_files(&plan, &rewind_id).await?;
        let commit = history::checkpoint::RewindCommit {
            rewind_id: rewind_id.clone(),
            cut_after: plan::cut_after(&plan.entries, plan.keep),
            conversation: request.scope.conversation(),
            files_restored: plan.restores.len(),
        };
        if let Err(e) = self
            .record_rewind(history::checkpoint::COMMIT, &serde_json::json!(commit))
            .await
        {
            let mut failure = self.undo(&rewind_id, undo, "the rewind's commit", e).await;
            failure.commit_not_recorded = true;
            return Err(RewindError::Failed(Box::new(failure)));
        }
        self.reload_after_rewind(&request, &plan).await?;
        let mut report = self.rewind_report(&request, &plan, &rewind_id, false);
        if let Some(conversation) = report.conversation.as_mut() {
            conversation.messages_before = before;
        }
        Ok(report)
    }

    fn checkpoints_for(
        &self,
        request: &RewindRequest,
    ) -> Result<Option<Arc<Checkpoints>>, RewindError> {
        if self.identity.agent_depth() > 0 {
            return Err(RewindError::Invalid(
                "this is a delegate's session; rewind its root session".into(),
            ));
        }
        if matches!(request.target, RewindTarget::ToToolUse(_)) && request.scope.conversation() {
            return Err(RewindError::Invalid(
                "to_tool_use cuts a turn in the middle, so it can only rewind files".into(),
            ));
        }
        let checkpoints = self.tooling.checkpoints().cloned();
        if request.scope.files() && checkpoints.is_none() {
            return Err(RewindError::Invalid(
                "this session keeps no file checkpoints".into(),
            ));
        }
        Ok(checkpoints)
    }

    async fn plan_rewind(&mut self, request: &RewindRequest) -> Result<Plan, RewindError> {
        let checkpoints = self.checkpoints_for(request)?;
        let store = self
            .session
            .history_store()
            .cloned()
            .ok_or_else(|| RewindError::Invalid("this session keeps no log".into()))?;
        self.flush_log().await.map_err(RewindError::Invalid)?;
        let sid = base::session::SessionId::parse(&self.session.session_id)
            .map_err(|e| RewindError::Invalid(e.to_string()))?;
        let entries = store
            .load(sid)
            .await
            .map_err(|e| RewindError::Invalid(e.to_string()))?;
        let voided = history::transcript::voided(&entries);
        let keep = plan::cut(&entries, &voided, &request.target)
            .map_err(|e| RewindError::Invalid(e.to_string()))?;
        let mut plan = Plan {
            entries,
            keep,
            restores: Vec::new(),
            issues: Vec::new(),
            overwrote_external: Vec::new(),
        };
        if let (true, Some(checkpoints)) = (request.scope.files(), checkpoints) {
            let policy = tools::security::write_policy(&self.context_outside_a_call(
                &tokio_util::sync::CancellationToken::new(),
                "rewind".into(),
            ))
            .await;
            let fs = &*self.exec.filesystem;
            let pruned = plan::pruned_through(&plan.entries).is_some();
            for step in plan::file_steps(&plan.entries, keep) {
                match preflight(fs, &checkpoints, &policy, &step, pruned).await {
                    Ok(restore) => {
                        let changed = changed_since(fs, &step, &restore.target).await;
                        if changed.is_none() || request.force {
                            if changed.is_some() {
                                plan.overwrote_external.push(restore.shown.clone());
                            }
                            plan.restores.push(restore);
                        }
                        plan.issues.extend(changed);
                    }
                    Err(issue) => plan.issues.push(issue),
                }
            }
        }
        Ok(plan)
    }

    async fn record_rewind(&self, event: &str, payload: &serde_json::Value) -> Result<(), String> {
        let store = self.session.history_store().ok_or("no log")?;
        let sid =
            base::session::SessionId::parse(&self.session.session_id).map_err(|e| e.to_string())?;
        store
            .append(
                sid,
                LogEntry::Extension {
                    ns: REWIND_NS.into(),
                    event: event.into(),
                    payload: payload.clone(),
                },
            )
            .await
            .map_err(|e| e.to_string())?;
        store.sync(sid).await.map_err(|e| e.to_string())
    }

    fn failed_before_any(&self, rewind_id: &str, error: String) -> RewindError {
        RewindError::Failed(Box::new(RewindFailure {
            rewind_id: rewind_id.to_string(),
            failed: FailedPath {
                path: String::new(),
                error: format!("could not record the rewind: {error}"),
            },
            rolled_back: Vec::new(),
            not_rolled_back: Vec::new(),
            partial: false,
            commit_not_recorded: false,
            conversation_changed: false,
        }))
    }

    async fn restore_files(&self, plan: &Plan, rewind_id: &str) -> Result<Vec<Undo>, RewindError> {
        let Some(checkpoints) = self.tooling.checkpoints() else {
            return Ok(Vec::new());
        };
        let fs = self.exec.filesystem.clone();
        let mut done = Vec::new();
        for restore in &plan.restores {
            let prior = fs.read(&restore.target).await.ok();
            let written = write_recorded(
                checkpoints,
                &fs,
                &restore.target,
                restore.to.as_deref(),
                rewind_id,
            )
            .await;
            match written {
                Ok(()) => done.push(Undo {
                    shown: restore.shown.clone(),
                    target: restore.target.clone(),
                    prior,
                }),
                Err(e) => {
                    let failure = self.undo(rewind_id, done, &restore.shown, e).await;
                    return Err(RewindError::Failed(Box::new(failure)));
                }
            }
        }
        Ok(done)
    }

    /// Put back what this rewind restored, newest first, and write `abort`.
    async fn undo(
        &self,
        rewind_id: &str,
        done: Vec<Undo>,
        failed: &str,
        error: String,
    ) -> RewindFailure {
        let fs = self.exec.filesystem.clone();
        let (mut rolled_back, mut not_rolled_back) = (Vec::new(), Vec::new());
        if let Some(checkpoints) = self.tooling.checkpoints() {
            for undo in done.into_iter().rev() {
                match write_recorded(
                    checkpoints,
                    &fs,
                    &undo.target,
                    undo.prior.as_deref(),
                    rewind_id,
                )
                .await
                {
                    Ok(()) => rolled_back.push(undo.shown),
                    Err(_) => not_rolled_back.push(undo.shown),
                }
            }
        }
        let abort = history::checkpoint::RewindAbort {
            rewind_id: rewind_id.to_string(),
            failed: format!("{failed}: {error}"),
            rolled_back: rolled_back.clone(),
            not_rolled_back: not_rolled_back.clone(),
        };
        if let Err(e) = self
            .record_rewind(history::checkpoint::ABORT, &serde_json::json!(abort))
            .await
        {
            tracing::warn!(error = %e, "failed to record a rewind's abort");
        }
        RewindFailure {
            rewind_id: rewind_id.to_string(),
            failed: FailedPath {
                path: failed.to_string(),
                error,
            },
            partial: !not_rolled_back.is_empty(),
            rolled_back,
            not_rolled_back,
            commit_not_recorded: false,
            conversation_changed: false,
        }
    }

    async fn reload_after_rewind(
        &mut self,
        request: &RewindRequest,
        plan: &Plan,
    ) -> Result<(), RewindError> {
        let restored: Vec<PathBuf> = plan
            .restores
            .iter()
            .flat_map(|r| [r.target.clone(), self.config_working_dir().join(&r.shown)])
            .collect();
        let shown: Vec<&str> = plan.restores.iter().map(|r| r.shown.as_str()).collect();
        if !request.scope.conversation() {
            self.session_state.forget_reads(Some(&restored));
            if !shown.is_empty() {
                self.session_state.post_notice(
                    Some("rewind".into()),
                    format!(
                        "The host restored these files to an earlier state: {}. Read a file again before editing it.",
                        shown.join(", ")
                    ),
                );
            }
            return Ok(());
        }
        let store = self.session.history_store().cloned();
        let entries = match store {
            Some(store) => store
                .load(
                    base::session::SessionId::parse(&self.session.session_id)
                        .map_err(|e| RewindError::Invalid(e.to_string()))?,
                )
                .await
                .map_err(|e| RewindError::Invalid(e.to_string()))?,
            None => return Ok(()),
        };
        let id = self.session.session_id.clone();
        self.load_transcript(&id, &entries)
            .map_err(|e| RewindError::Invalid(e.to_string()))?;
        let durable = session::transcript::latest_extension(
            &entries,
            base::context::SESSION_STATE_NS,
            base::context::SESSION_STATE_EVENT,
        )
        .and_then(|payload| serde_json::from_value(payload).ok());
        self.session_state.restore_rewound_state(durable);
        self.session_state.forget_reads(None);
        if !request.scope.files() {
            let written: Vec<String> = plan::file_steps(&plan.entries, plan.keep)
                .into_iter()
                .map(|s| s.path.as_str().to_string())
                .collect();
            if !written.is_empty() {
                self.session_state.post_notice(
                    Some("rewind".into()),
                    format!(
                        "The host took back the most recent turns of this conversation. The files those turns changed were left as they are now: {}.",
                        written.join(", ")
                    ),
                );
            }
        }
        Ok(())
    }

    fn config_working_dir(&self) -> PathBuf {
        self.settings.paths.working_dir().to_path_buf()
    }

    fn rewind_report(
        &self,
        request: &RewindRequest,
        plan: &Plan,
        rewind_id: &str,
        dry_run: bool,
    ) -> RewindReport {
        let conversation = request.scope.conversation().then(|| {
            let voided = history::transcript::voided(&plan.entries);
            let removed_turns = plan::live_turns(&plan.entries, &voided)
                .into_iter()
                .filter(|t| t.first >= plan.keep)
                .map(|t| RemovedTurn {
                    turn_id: t.turn_id,
                    prompt: t.prompt,
                })
                .collect();
            let kept = history::transcript::project_messages_with(
                &plan.entries[..plan.keep],
                &history::transcript::DefaultProjection,
            )
            .len();
            RewoundConversation {
                messages_before: self.session.messages().len(),
                messages_after: if dry_run {
                    kept
                } else {
                    self.session.messages().len()
                },
                removed_turns,
            }
        });
        let files = request.scope.files().then(|| RewoundFiles {
            restored: plan.restores.iter().map(restored_file).collect(),
            skipped: plan
                .issues
                .iter()
                .filter(|i| i.issue != RewindIssueKind::Modified)
                .cloned()
                .collect(),
            overwrote_external: plan.overwrote_external.clone(),
        });
        RewindReport {
            rewind_id: rewind_id.to_string(),
            dry_run,
            conversation,
            files,
        }
    }
}

fn overridden(issue: &RewindIssue, request: &RewindRequest) -> bool {
    match issue.issue {
        RewindIssueKind::Modified => request.force,
        _ => request.allow_partial,
    }
}

fn begin_payload(request: &RewindRequest, plan: &Plan, rewind_id: &str) -> serde_json::Value {
    serde_json::json!(history::checkpoint::RewindBegin {
        rewind_id: rewind_id.to_string(),
        target: request.target.clone(),
        scope: request.scope,
        cut_after: plan::cut_after(&plan.entries, plan.keep),
        paths: plan.restores.iter().map(|r| r.shown.clone()).collect(),
    })
}

fn restored_file(restore: &Restore) -> RestoredFile {
    RestoredFile {
        path: restore.shown.clone(),
        to: if restore.to.is_some() {
            "file"
        } else {
            "absent"
        }
        .into(),
        sha256: restore.to.as_deref().map(history::checkpoint::sha256_hex),
        outside_project: restore.outside_project,
        unverified: restore.unverified,
    }
}

fn issue(step: &plan::FileStep, kind: RewindIssueKind, detail: impl Into<String>) -> RewindIssue {
    let overridable_by = if kind == RewindIssueKind::Modified {
        "force"
    } else {
        "allow_partial"
    };
    RewindIssue {
        path: step.path.as_str().to_string(),
        issue: kind,
        detail: detail.into(),
        overridable_by: overridable_by.into(),
    }
}

/// Something other than the session changed `target` since the session last
/// wrote it. A write with no `post` has nothing to compare with, so any file
/// there counts as changed: only `force` overwrites it.
async fn changed_since(
    fs: &dyn FileSystem,
    step: &plan::FileStep,
    target: &Path,
) -> Option<RewindIssue> {
    let now = crate::checkpoint::current_state(fs, target).await;
    let Some(expected) = step.expected_now.as_ref() else {
        return (now != After::Absent).then(|| {
            issue(
                step,
                RewindIssueKind::Modified,
                "the session's last write of it left no record of what it wrote, so it cannot be told from a change made since",
            )
        });
    };
    (&now != expected).then(|| {
        issue(
            step,
            RewindIssueKind::Modified,
            describe_change(expected, &now),
        )
    })
}

/// Whether `step` can be restored as recorded, and what it would write.
async fn preflight(
    fs: &dyn FileSystem,
    checkpoints: &Checkpoints,
    policy: &permissions::path_safety::WritePolicy,
    step: &plan::FileStep,
    pruned: bool,
) -> Result<Restore, RewindIssue> {
    let target = step
        .path
        .resolve(checkpoints.project_root())
        .ok_or_else(|| {
            issue(
                step,
                RewindIssueKind::Unavailable,
                "recorded relative to a project this session does not have",
            )
        })?;
    let to = recorded_contents(checkpoints, step, pruned)?;
    let now = fs.metadata(&target).await.ok();
    if let Some(len) = now
        .as_ref()
        .filter(|m| m.is_file && m.len > checkpoints.max_file_bytes())
        .map(|m| m.len)
    {
        return Err(issue(
            step,
            RewindIssueKind::Unavailable,
            format!("it is {len} bytes now, too large to keep a copy of before overwriting"),
        ));
    }
    if now.is_some_and(|m| m.is_symlink) {
        return Err(issue(
            step,
            RewindIssueKind::Symlink,
            "it is a symbolic link now; links are never followed",
        ));
    }
    if let Some(parent) = target.parent() {
        if fs.canonicalize_best_effort(parent).await != parent {
            return Err(issue(
                step,
                RewindIssueKind::Symlink,
                "a directory above it resolves somewhere else now",
            ));
        }
    }
    if let Some(root) = removed_worktree(fs, &target).await {
        return Err(issue(
            step,
            RewindIssueKind::WorktreeRemoved,
            format!("{} is gone", root.display()),
        ));
    }
    use permissions::path_safety::{check_write_for, PathSafetyError};
    // The NFC check is about how a model spelled a path; this one comes from
    // the log, and the checks after it must still run.
    let checked = match check_write_for(&target, &target, policy) {
        Err(PathSafetyError::UnicodeNormalizationAttack { normalized, .. }) => {
            check_write_for(&normalized, &target, policy)
        }
        other => other,
    };
    let outside_project = match checked {
        Ok(()) => false,
        Err(PathSafetyError::OutsideAllowedRoots { .. }) => true,
        Err(e) => return Err(issue(step, RewindIssueKind::Denied, e.to_string())),
    };
    Ok(Restore {
        shown: step.path.as_str().to_string(),
        target,
        to,
        outside_project: outside_project || step.path.outside_project(),
        unverified: step.expected_now.is_none(),
    })
}

/// What `step` restores the file to: its earlier bytes, or `None` for
/// "it did not exist".
fn recorded_contents(
    checkpoints: &Checkpoints,
    step: &plan::FileStep,
    pruned: bool,
) -> Result<Option<Vec<u8>>, RewindIssue> {
    let unavailable = |detail: String| issue(step, RewindIssueKind::Unavailable, detail);
    match &step.restore {
        Before::Absent => Ok(None),
        Before::File { sha256, .. } => match checkpoints.objects().get(sha256) {
            Ok(Some(bytes)) => Ok(Some(bytes)),
            _ if pruned => Err(unavailable(
                "its earlier contents were pruned to stay under checkpoints.max_total_bytes, or are no longer kept".into(),
            )),
            _ => Err(unavailable(
                "its earlier contents are no longer kept".into(),
            )),
        },
        Before::TooLarge { bytes, .. } => {
            Err(unavailable(format!("{bytes} bytes was too large to keep")))
        }
        Before::Unreadable { error } => Err(unavailable(format!("it could not be read: {error}"))),
        Before::NotAFile => Err(unavailable("it was not a regular file".into())),
    }
}

fn describe_change(expected: &After, now: &After) -> String {
    match (expected, now) {
        (_, After::Absent) => "it was deleted since the session last wrote it".into(),
        (After::Absent, _) => "it was created since the session last removed it".into(),
        _ => "it was changed since the session last wrote it".into(),
    }
}

/// The worktree `target` was written in, when that worktree is gone.
async fn removed_worktree(fs: &dyn FileSystem, target: &Path) -> Option<PathBuf> {
    let parts: Vec<_> = target.components().collect();
    let at = parts
        .windows(2)
        .position(|w| w[0].as_os_str() == ".agix" && w[1].as_os_str() == "worktrees")?;
    let root: PathBuf = parts.get(..at + 3)?.iter().collect();
    match fs.metadata(&root).await {
        Ok(_) => None,
        Err(_) => Some(root),
    }
}

/// Write `to` (or remove the file, for `None`) as a write of its own, with a
/// `pre` and `post` like any file tool's.
async fn write_recorded(
    checkpoints: &Checkpoints,
    fs: &Arc<dyn FileSystem>,
    target: &Path,
    to: Option<&[u8]>,
    rewind_id: &str,
) -> Result<(), String> {
    let by = WriteBy {
        tool: "rewind",
        tool_use_id: None,
        rewind_id: Some(rewind_id.to_string()),
        turn_id: None,
    };
    let pending = checkpoints
        .before_write(fs.clone(), target.to_path_buf(), by)
        .await?;
    let written = match to {
        Some(bytes) => {
            if let Some(parent) = target.parent() {
                let _ = fs.create_dir_all(parent).await;
            }
            fs.write_not_following(target, bytes).await
        }
        None if fs.metadata(target).await.is_ok() => fs.remove_file(target).await,
        None => Ok(()),
    };
    pending.finish(written.is_ok()).await;
    written.map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use history::checkpoint::RecordedPath;

    /// What a rewind writes is the file it checked, not whatever a link put
    /// in its place since: a file swapped for a symlink between the check
    /// and the write is not written through.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_file_swapped_for_a_symlink_after_the_check_is_not_written_through() {
        let dir = tempfile::tempdir().unwrap();
        let project = std::fs::canonicalize(dir.path()).unwrap();
        let outside = project.join("outside.txt");
        std::fs::write(&outside, "keep me").unwrap();
        let target = project.join("a.txt");
        std::os::unix::fs::symlink(&outside, &target).unwrap();
        let store: Arc<dyn history::store::HistoryStore> =
            Arc::new(history::store::InMemoryHistoryStore::new());
        let checkpoints = Checkpoints::for_root(
            store,
            &base::session::SessionId::new().to_string(),
            Some(project.clone()),
            Default::default(),
        )
        .unwrap();
        let fs: Arc<dyn FileSystem> = Arc::new(platform::filesystem::LocalFileSystem);
        let wrote = write_recorded(&checkpoints, &fs, &target, Some(b"restored"), "r").await;
        assert!(wrote.is_err(), "{wrote:?}");
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "keep me");
    }

    /// A write that left no `post` has nothing to compare the file with, so a
    /// file standing there is overwritten only with `force`.
    #[tokio::test]
    async fn a_write_without_a_post_counts_as_changed_unless_the_file_is_gone() {
        use base::interface::exec::in_process::InMemoryFileSystem;
        let fs = InMemoryFileSystem::new().with_file("/p/a.txt", "whatever");
        let step = plan::FileStep {
            path: RecordedPath::Rel("a.txt".into()),
            restore: Before::Absent,
            expected_now: None,
            tools: vec!["Write".into()],
        };
        let changed = changed_since(&fs, &step, Path::new("/p/a.txt")).await;
        assert!(
            matches!(&changed, Some(i) if i.issue == RewindIssueKind::Modified && i.overridable_by == "force")
        );
        assert!(changed_since(&fs, &step, Path::new("/p/gone.txt"))
            .await
            .is_none());
    }

    /// A project whose path is not in NFC — a folder Finder made, on macOS —
    /// still has its settings and secrets kept from a rewind.
    #[tokio::test]
    async fn a_path_not_in_nfc_still_meets_the_deny_list() {
        let dir = tempfile::tempdir().unwrap();
        let project = std::fs::canonicalize(dir.path())
            .unwrap()
            .join("De\u{301}veloppement");
        std::fs::create_dir_all(project.join(".agix")).unwrap();
        let store: Arc<dyn history::store::HistoryStore> =
            Arc::new(history::store::InMemoryHistoryStore::new());
        let checkpoints = Checkpoints::for_root(
            store,
            &base::session::SessionId::new().to_string(),
            Some(project.clone()),
            Default::default(),
        )
        .unwrap();
        let policy = permissions::path_safety::WritePolicy::new(project.clone());
        for rel in [".agix/settings.json", ".env"] {
            let step = plan::FileStep {
                path: RecordedPath::Rel(rel.into()),
                restore: Before::Absent,
                expected_now: None,
                tools: vec!["Write".into()],
            };
            let got = preflight(
                &platform::filesystem::LocalFileSystem,
                &checkpoints,
                &policy,
                &step,
                false,
            )
            .await;
            assert!(
                matches!(&got, Err(issue) if issue.issue == RewindIssueKind::Denied),
                "{rel}: {:?}",
                got.map(|r| r.target)
            );
        }
        let ordinary = plan::FileStep {
            path: RecordedPath::Rel("notes.txt".into()),
            restore: Before::Absent,
            expected_now: None,
            tools: vec!["Write".into()],
        };
        assert!(preflight(
            &platform::filesystem::LocalFileSystem,
            &checkpoints,
            &policy,
            &ordinary,
            false
        )
        .await
        .is_ok());
    }
}
