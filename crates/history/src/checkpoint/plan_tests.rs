use super::*;
use crate::checkpoint::{PreFile, RewindCommit, RewindScope, BEGIN, COMMIT};
use crate::transcript::{
    model_visible_content_is_reconstructible, project_messages, projected_lengths, voided,
    DefaultProjection,
};
use base::message::Message;
use base::session::SessionId;
use serde_json::json;

struct Log {
    sid: SessionId,
    entries: Vec<EnvelopedEntry>,
    next: usize,
}

fn text(s: &str) -> Vec<ContentBlock> {
    vec![ContentBlock::Text {
        text: s.into(),
        cache_control: None,
    }]
}

fn file(sha: &str) -> Before {
    Before::File {
        sha256: sha.into(),
        bytes: 1,
    }
}

impl Log {
    fn new() -> Self {
        Self {
            sid: SessionId::new(),
            entries: Vec::new(),
            next: 0,
        }
    }

    fn push(&mut self, turn: Option<&str>, origin: Option<MessageOrigin>, entry: LogEntry) {
        let tags = crate::entry::EntryTags {
            turn_id: turn.map(str::to_string),
            origin,
        };
        self.entries
            .push(EnvelopedEntry::new(self.sid, entry).with_tags(tags));
    }

    fn ext(&mut self, turn: Option<&str>, ns: &str, event: &str, payload: serde_json::Value) {
        self.push(
            turn,
            None,
            LogEntry::Extension {
                ns: ns.into(),
                event: event.into(),
                payload,
            },
        );
    }

    fn turn(&mut self, turn: &str, prompt: &str, reply: &str) {
        self.push(
            Some(turn),
            Some(MessageOrigin::User),
            LogEntry::User {
                content: text(prompt),
            },
        );
        self.push(
            Some(turn),
            None,
            LogEntry::Assistant {
                content: text(reply),
                stop_reason: None,
                usage: None,
                model: None,
            },
        );
    }

    fn write(
        &mut self,
        turn: Option<&str>,
        tool_use: &str,
        path: &str,
        before: Before,
        after: Option<After>,
    ) {
        self.next += 1;
        let id = format!("w{}", self.next);
        let pre = PreRecord {
            id: id.clone(),
            files: vec![PreFile {
                path: RecordedPath::Rel(path.into()),
                before,
            }],
            tool: "Write".into(),
            tool_use_id: Some(tool_use.into()),
            rewind_id: None,
            agent: None,
        };
        self.ext(turn, CHECKPOINT_NS, PRE, serde_json::to_value(pre).unwrap());
        if let Some(after) = after {
            let post = PostRecord {
                pre: id,
                ok: true,
                after: vec![after],
            };
            self.ext(
                turn,
                CHECKPOINT_NS,
                POST,
                serde_json::to_value(post).unwrap(),
            );
        }
    }

    fn commit(&mut self, keep: usize, conversation: bool) {
        let commit = RewindCommit {
            rewind_id: format!("r{}", self.entries.len()),
            cut_after: cut_after(&self.entries, keep),
            conversation,
            files_restored: 0,
        };
        self.ext(
            None,
            REWIND_NS,
            COMMIT,
            serde_json::to_value(commit).unwrap(),
        );
    }

    fn keep(&self, target: RewindTarget) -> usize {
        cut(&self.entries, &voided(&self.entries), &target).unwrap()
    }

    fn restores(&self, keep: usize) -> Vec<(String, Before)> {
        file_steps(&self.entries, keep)
            .into_iter()
            .map(|s| (s.path.as_str().to_string(), s.restore))
            .collect()
    }

    fn texts(&self) -> Vec<String> {
        project_messages(&self.entries)
            .iter()
            .map(|m| match m {
                Message::User { content } | Message::Assistant { content, .. } => content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::Text { text, .. } => Some(text.clone()),
                        _ => None,
                    })
                    .collect::<String>(),
                _ => String::new(),
            })
            .collect()
    }
}

fn to_turn(t: &str) -> RewindTarget {
    RewindTarget::ToTurn(t.into())
}

#[test]
fn two_writes_to_one_file_restore_what_was_there_before_the_first() {
    let mut log = Log::new();
    log.turn("t1", "one", "done one");
    log.turn("t2", "two", "done two");
    log.write(
        Some("t2"),
        "a",
        "a.txt",
        file("v1"),
        Some(After::File {
            sha256: "v2".into(),
        }),
    );
    log.write(
        Some("t2"),
        "b",
        "a.txt",
        file("v2"),
        Some(After::File {
            sha256: "v3".into(),
        }),
    );
    let keep = log.keep(to_turn("t1"));
    let steps = file_steps(&log.entries, keep);
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0].restore, file("v1"));
    assert_eq!(
        steps[0].expected_now,
        Some(After::File {
            sha256: "v3".into()
        }),
        "checked against what the last write left"
    );
    assert_eq!(steps[0].tools, ["Write", "Write"]);
}

#[test]
fn a_file_the_cut_created_is_restored_to_absent_and_earlier_writes_are_not_touched() {
    let mut log = Log::new();
    log.turn("t1", "one", "done one");
    log.write(Some("t1"), "a", "kept.txt", Before::Absent, None);
    log.turn("t2", "two", "done two");
    log.write(
        Some("t2"),
        "b",
        "new.txt",
        Before::Absent,
        Some(After::File { sha256: "n".into() }),
    );
    let keep = log.keep(to_turn("t1"));
    assert_eq!(log.restores(keep), [("new.txt".into(), Before::Absent)]);
}

#[test]
fn a_write_without_a_post_has_nothing_to_check_against() {
    let mut log = Log::new();
    log.turn("t1", "one", "done one");
    log.write(Some("t2"), "a", "a.txt", file("v1"), None);
    let steps = file_steps(&log.entries, log.keep(to_turn("t1")));
    assert_eq!(steps[0].expected_now, None);
}

#[test]
fn writes_in_a_turn_already_rewound_still_count_for_the_files() {
    let mut log = Log::new();
    log.turn("t1", "one", "done one");
    log.write(
        Some("t1"),
        "w1",
        "a.txt",
        Before::Absent,
        Some(After::File {
            sha256: "v1".into(),
        }),
    );
    log.turn("t2", "two", "done two");
    log.write(
        Some("t2"),
        "w2",
        "a.txt",
        file("v1"),
        Some(After::File {
            sha256: "v2".into(),
        }),
    );
    log.turn("t3", "three", "done three");
    log.write(
        Some("t3"),
        "w3",
        "b.txt",
        Before::Absent,
        Some(After::File {
            sha256: "b3".into(),
        }),
    );
    log.write(
        Some("t3"),
        "w4",
        "a.txt",
        file("v2"),
        Some(After::File {
            sha256: "v3".into(),
        }),
    );

    let keep = log.keep(to_turn("t2"));
    assert_eq!(
        log.restores(keep),
        [
            ("a.txt".into(), file("v2")),
            ("b.txt".into(), Before::Absent)
        ]
    );
    log.write(
        None,
        "r",
        "a.txt",
        file("v3"),
        Some(After::File {
            sha256: "v2".into(),
        }),
    );
    log.write(None, "r", "b.txt", file("b3"), Some(After::Absent));
    log.commit(keep, true);
    assert_eq!(log.texts().last().map(String::as_str), Some("done two"));

    let keep = log.keep(to_turn("t1"));
    let steps = file_steps(&log.entries, keep);
    assert_eq!(
        log.restores(keep),
        [
            ("a.txt".into(), file("v1")),
            ("b.txt".into(), Before::Absent)
        ],
        "b.txt was written only in t3, which the first rewind voided"
    );
    assert_eq!(
        steps[0].expected_now,
        Some(After::File {
            sha256: "v2".into()
        }),
        "a.txt is what the first rewind left it"
    );
    log.commit(keep, true);
    assert_eq!(log.texts(), ["one", "done one"]);
}

#[test]
fn a_rewind_of_the_conversation_only_leaves_the_writes_for_a_later_one() {
    let mut log = Log::new();
    log.turn("t1", "one", "done one");
    log.turn("t2", "two", "done two");
    log.write(
        Some("t2"),
        "w",
        "a.txt",
        Before::Absent,
        Some(After::File { sha256: "v".into() }),
    );
    let keep = log.keep(to_turn("t1"));
    log.commit(keep, true);
    assert_eq!(
        log.restores(log.keep(RewindTarget::ToStart(true))),
        [("a.txt".into(), Before::Absent)]
    );
}

#[test]
fn a_rewind_of_the_files_only_leaves_the_conversation() {
    let mut log = Log::new();
    log.turn("t1", "one", "done one");
    log.turn("t2", "two", "done two");
    let keep = log.keep(to_turn("t1"));
    log.commit(keep, false);
    assert_eq!(log.texts().len(), 4);
    assert_eq!(log.keep(to_turn("t2")), 4);
}

#[test]
fn a_tool_use_cut_covers_the_calls_a_program_made_under_it() {
    let mut log = Log::new();
    log.turn("t1", "one", "done one");
    log.write(Some("t1"), "toolu_1", "a.txt", file("a0"), None);
    log.write(Some("t1"), "toolu_2.1", "b.txt", file("b0"), None);
    log.write(Some("t1"), "toolu_2.2", "c.txt", file("c0"), None);
    let keep = log.keep(RewindTarget::ToToolUse("toolu_2".into()));
    assert_eq!(
        log.restores(keep),
        [("b.txt".into(), file("b0")), ("c.txt".into(), file("c0"))]
    );
    assert_eq!(
        cut(
            &log.entries,
            &voided(&log.entries),
            &RewindTarget::ToToolUse("toolu_9".into())
        ),
        Err(CutError::NoSuchToolUse("toolu_9".into()))
    );
}

#[test]
fn a_turn_rewound_already_or_never_run_is_no_target() {
    let mut log = Log::new();
    log.turn("t1", "one", "done one");
    log.turn("t2", "two", "done two");
    let keep = log.keep(to_turn("t1"));
    log.commit(keep, true);
    let v = voided(&log.entries);
    assert_eq!(
        cut(&log.entries, &v, &to_turn("t2")),
        Err(CutError::TurnRewound("t2".into()))
    );
    assert_eq!(
        cut(&log.entries, &v, &to_turn("t9")),
        Err(CutError::NoSuchTurn("t9".into()))
    );
}

#[test]
fn a_compaction_inside_the_rewound_range_is_undone_with_it() {
    let mut log = Log::new();
    log.turn("t1", "one", "done one");
    log.turn("t2", "two", "done two");
    log.push(
        Some("t2"),
        None,
        LogEntry::Compact {
            before_tokens: 10,
            after_tokens: 1,
            summary_block_id: None,
            replacement_history: Some(vec![Message::User {
                content: text("summary"),
            }]),
            summary: None,
            snip_removed_uuids: None,
        },
    );
    assert_eq!(log.texts(), ["summary"]);
    let keep = log.keep(to_turn("t1"));
    log.commit(keep, true);
    assert_eq!(log.texts(), ["one", "done one"]);
}

#[test]
fn the_goal_and_a_hosts_entries_survive_a_rewind_and_the_state_does_not() {
    let mut log = Log::new();
    log.turn("t1", "one", "done one");
    log.turn("t2", "two", "done two");
    log.ext(Some("t2"), "agix.goal", "goal", json!({"goal": "g"}));
    log.ext(
        Some("t2"),
        base::context::SESSION_STATE_NS,
        "state",
        json!({}),
    );
    log.ext(None, "host.notes", "note", json!({}));
    log.ext(
        Some("t2"),
        crate::transcript::CONTEXT_NS,
        "drop",
        json!({"at": 0}),
    );
    let keep = log.keep(to_turn("t1"));
    log.commit(keep, true);
    let v = voided(&log.entries);
    let kept: Vec<_> = log
        .entries
        .iter()
        .zip(&v)
        .filter(|(_, void)| !**void)
        .filter_map(|(e, _)| match &e.entry {
            LogEntry::Extension { ns, .. } => Some(ns.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(kept, ["agix.goal", "host.notes", REWIND_NS]);
    assert_eq!(log.texts(), ["one", "done one"], "the drop went with t2");
}

#[test]
fn nested_rewinds_void_the_union_of_their_ranges() {
    let mut log = Log::new();
    log.turn("t1", "one", "done one");
    log.turn("t2", "two", "done two");
    log.turn("t3", "three", "done three");
    let keep = log.keep(to_turn("t2"));
    log.commit(keep, true);
    log.turn("t4", "four", "done four");
    let keep = log.keep(to_turn("t1"));
    log.commit(keep, true);
    assert_eq!(log.texts(), ["one", "done one"]);
    log.turn("t5", "five", "done five");
    assert_eq!(log.texts(), ["one", "done one", "five", "done five"]);
}

#[test]
fn every_projection_entry_point_agrees_on_a_rewound_log() {
    let mut log = Log::new();
    log.turn("t1", "one", "done one");
    log.turn("t2", "two", "done two");
    let keep = log.keep(to_turn("t1"));
    log.commit(keep, true);
    log.turn("t3", "three", "done three");
    let lengths = projected_lengths(&log.entries);
    assert_eq!(
        lengths.last().copied(),
        Some(project_messages(&log.entries).len())
    );
    assert_eq!(
        crate::transcript::latest_projected_message_entry_id(&log.entries),
        log.entries.last().map(|e| e.id)
    );
    let sourced = crate::transcript::project_messages_sourced(&log.entries, &DefaultProjection);
    assert_eq!(sourced.len(), 4);
    assert!(model_visible_content_is_reconstructible(
        &DefaultProjection,
        &log.entries
    ));
}

#[test]
fn the_rewind_menu_lists_live_turns_with_what_each_wrote() {
    let mut log = Log::new();
    log.turn("t1", "one", "done one");
    log.write(Some("t1"), "a", "a.txt", Before::Absent, None);
    log.write(None, "sub", "c.txt", Before::Absent, None);
    log.turn("t2", "two", "done two");
    log.write(Some("t2"), "b", "b.txt", Before::Absent, None);
    let turns = live_turns(&log.entries, &voided(&log.entries));
    let summary: Vec<(String, Vec<String>)> = turns
        .iter()
        .map(|t| {
            (
                t.turn_id.clone(),
                t.files
                    .iter()
                    .map(|f| f.path.as_str().to_string())
                    .collect(),
            )
        })
        .collect();
    assert_eq!(
        summary,
        [
            ("t1".into(), vec!["a.txt".into(), "c.txt".into()]),
            ("t2".into(), vec!["b.txt".into()])
        ]
    );
    assert_eq!(turns[1].prompt, text("two"));
}

#[test]
fn a_rewind_that_began_and_never_ended_is_reported() {
    let mut log = Log::new();
    log.turn("t1", "one", "done one");
    let begin = RewindBegin {
        rewind_id: "r1".into(),
        target: to_turn("t1"),
        scope: RewindScope::Files,
        cut_after: None,
        paths: Vec::new(),
    };
    log.ext(
        None,
        REWIND_NS,
        BEGIN,
        serde_json::to_value(&begin).unwrap(),
    );
    assert_eq!(
        interrupted_rewind(&log.entries)
            .map(|(id, _)| id)
            .as_deref(),
        Some("r1")
    );
    log.ext(
        None,
        REWIND_NS,
        COMMIT,
        json!({"rewind_id": "r1", "cut_after": null, "conversation": false, "files_restored": 0}),
    );
    assert_eq!(interrupted_rewind(&log.entries), None);
}
