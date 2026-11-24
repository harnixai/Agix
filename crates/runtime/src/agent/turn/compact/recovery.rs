//! 压缩之后：放回模型面前的恢复上下文，以及结果已不在上下文里的 `Read` 不再以"文件未变"作答。

use super::super::process::running_task_summary;
use crate::agent::Agent;
use base::context::Session;
use base::interface::model::ModelMessage;
use std::collections::HashSet;

impl Agent {
    /// What goes back in front of what a compaction `kept` of `pre_compact`:
    /// the files whose reads it dropped, the skills invoked, the deferred
    /// tools loaded, and — when it dropped turns — plan mode and the
    /// background tasks still running.
    pub(super) fn post_compact_recovery(
        &self,
        pre_compact: &[ModelMessage],
        kept: &[ModelMessage],
        drops_history: bool,
    ) -> Vec<ModelMessage> {
        let still_there: HashSet<(String, String)> =
            compaction::compact::extract_recent_reads(kept)
                .into_iter()
                .collect();
        let recent_files = compaction::compact::extract_recent_reads(pre_compact)
            .into_iter()
            .filter(|read| !still_there.contains(read))
            .collect();
        // Each invoked skill's *current* content is resolved here because the
        // `compaction` crate does not depend on `skills`. A name that no
        // longer resolves still gets a record, so it is mentioned by name.
        let invoked_skills = self
            .session_state
            .ledger()
            .invoked_skills
            .clone()
            .into_iter()
            .map(|name| compaction::compact::InvokedSkillRecord {
                content: self.tooling.skills().body(&name),
                last_seq: self.tooling.skills().last_invoked_seq(&name),
                name,
            })
            .collect();
        let mut activated_tools: Vec<String> =
            self.session_state.activated_tools().into_iter().collect();
        activated_tools.sort();
        compaction::compact::build_post_compact_recovery(&compaction::compact::PostCompactContext {
            recent_files,
            invoked_skills,
            in_plan_mode: drops_history
                && self.session_state.permission_mode() == base::permission::PermissionMode::Plan,
            activated_tools,
            running_tasks: if drops_history {
                self.session_state
                    .running_background_tasks()
                    .iter()
                    .map(|task| (task.task_id.clone(), running_task_summary(task)))
                    .collect()
            } else {
                Vec::new()
            },
        })
    }

    /// What any compaction puts back whatever it drops — skills, plan mode,
    /// tasks, loaded tools. Taken off the target, so that the recovery does
    /// not carry the conversation straight back over the threshold. Files are
    /// left out: one is re-attached only when its read was dropped, and the
    /// read cost at least as much.
    pub(super) fn recovery_reserve(&self) -> usize {
        self.count(&self.post_compact_recovery(&[], &[], true))
    }

    /// Stop answering a repeat of a `Read` whose result `before` held and the
    /// transcript no longer does.
    pub(super) fn release_reads_lost(&self, before: &[ModelMessage]) {
        let lost = compaction::held_reads::reads_lost_from_view(before, self.session.messages());
        if !lost.is_empty() {
            self.session_state.release_reads(&lost);
        }
    }
}
