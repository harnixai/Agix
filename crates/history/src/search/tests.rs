use super::*;
use crate::entry::{EntryTags, MessageOrigin};
use crate::store::{HistoryStore, InMemoryHistoryStore};
use crate::transcript::DefaultProjection;
use base::message::ToolResultContent;

fn text(t: &str) -> Vec<ContentBlock> {
    vec![ContentBlock::Text {
        text: t.to_string(),
        cache_control: None,
    }]
}

fn said(sid: SessionId, origin: MessageOrigin, t: &str) -> EnvelopedEntry {
    EnvelopedEntry::new(sid, LogEntry::User { content: text(t) }).with_tags(EntryTags {
        turn_id: None,
        origin: Some(origin),
    })
}

fn replied(sid: SessionId, t: &str) -> EnvelopedEntry {
    EnvelopedEntry::new(
        sid,
        LogEntry::Assistant {
            content: text(t),
            stop_reason: None,
            usage: None,
            model: None,
        },
    )
}

fn tool_result(sid: SessionId, t: &str) -> EnvelopedEntry {
    EnvelopedEntry::new(
        sid,
        LogEntry::ToolResult {
            tool_use_id: "t1".into(),
            content: ToolResultContent::Text(t.into()),
            is_error: false,
        },
    )
}

fn sidechain_meta() -> LogEntry {
    LogEntry::Meta {
        cwd: "/tmp".into(),
        started_at: time::OffsetDateTime::now_utc(),
        model: "m".into(),
        permission_mode: "default".into(),
        engine_version: "0".into(),
        agix_version: "0".into(),
        parent_session_id: Some("parent".into()),
        scene: None,
        project_root: None,
        session_kind: SessionKind::Sidechain,
        schema_version: crate::entry::CURRENT_META_SCHEMA_VERSION,
    }
}

fn hit(entries: &[EnvelopedEntry], needle: &str) -> Option<(usize, MessageSource, String)> {
    first_hit(entries, &DefaultProjection, needle, 40).map(|(seq, source, _, s)| (seq, source, s))
}

#[test]
fn the_persons_words_and_the_models_replies_are_found_with_their_position() {
    let sid = SessionId::new();
    let log = vec![
        said(
            sid,
            MessageOrigin::User,
            "how do I pool postgres connections?",
        ),
        replied(sid, "Use PgBouncer in front of Postgres."),
    ];
    assert_eq!(
        hit(&log, "POSTGRES").map(|(seq, source, _)| (seq, source)),
        Some((0, MessageSource::User))
    );
    assert_eq!(
        hit(&log, "pgbouncer").map(|(seq, source, _)| (seq, source)),
        Some((1, MessageSource::Assistant))
    );
}

#[test]
fn steers_and_commands_count_and_injected_text_does_not() {
    let sid = SessionId::new();
    let log = vec![
        said(sid, MessageOrigin::Injected, "recalled memory: kafka"),
        said(sid, MessageOrigin::Steer, "actually use redis"),
        replied(sid, "ok"),
        said(sid, MessageOrigin::Command, "/review the kafka consumer"),
    ];
    assert_eq!(hit(&log, "redis").unwrap().1, MessageSource::Steer);
    let (seq, source, _) = hit(&log, "kafka").unwrap();
    assert_eq!((seq, source), (3, MessageSource::Command));
}

#[test]
fn reminders_inside_a_prompt_and_tool_results_are_not_searched() {
    let sid = SessionId::new();
    let log = vec![
        said(
            sid,
            MessageOrigin::User,
            "fix it <system-reminder>secret-token</system-reminder> please",
        ),
        replied(sid, "reading"),
        tool_result(sid, "file contents mention secret-token too"),
        said(
            sid,
            MessageOrigin::User,
            "trailing <system-reminder>open-ended",
        ),
    ];
    assert_eq!(hit(&log, "secret-token"), None);
    assert_eq!(hit(&log, "open-ended"), None);
    assert_eq!(hit(&log, "it please").map(|h| h.0), None);
    assert_eq!(hit(&log, "please").map(|h| h.0), Some(0));
}

#[test]
fn a_blank_needle_matches_nothing() {
    let sid = SessionId::new();
    let log = vec![said(sid, MessageOrigin::User, "anything")];
    assert_eq!(hit(&log, "  "), None);
}

#[test]
fn the_snippet_is_cut_around_the_match_with_whitespace_collapsed() {
    let long = format!("{}needle\n\n  here{}", "a".repeat(100), "b".repeat(100));
    let folded = Folded::of("needle");
    let snippet = snippet_of(&long, &folded, 20).unwrap();
    assert!(
        snippet.starts_with('…') && snippet.ends_with('…'),
        "{snippet}"
    );
    assert!(snippet.contains("aneedle her"), "{snippet}");
    assert!(snippet.chars().filter(|c| *c != '…').count() <= 20);

    assert_eq!(
        snippet_of("short needle", &folded, 120).unwrap(),
        "short needle"
    );
}

#[test]
fn a_match_whose_lowercase_changes_length_maps_back_to_the_original() {
    let folded = Folded::of("straße");
    assert_eq!(
        snippet_of("İİİ STRASSE straße end", &folded, 6).unwrap(),
        "…straße…"
    );
    let dotted = Folded::of("i̇x");
    assert_eq!(Folded::of("aİx").find(&dotted), Some((1, 3)));
}

#[tokio::test]
async fn the_default_scan_keeps_the_given_order_and_reports_what_stopped_it() {
    let store = InMemoryHistoryStore::new();
    let ids: Vec<SessionId> = (0..3).map(|_| SessionId::new()).collect();
    for (i, sid) in ids.iter().enumerate() {
        store
            .append_enveloped(
                *sid,
                vec![said(
                    *sid,
                    MessageOrigin::User,
                    &format!("topic {i} shared"),
                )],
            )
            .await
            .unwrap();
    }
    let query = |limit: usize| TextQuery {
        text: "shared".into(),
        sessions: ids.clone(),
        include_sidechains: false,
        limit,
        deadline: None,
        snippet_chars: 120,
    };

    let all = store.search_text(&query(10)).await.unwrap();
    assert_eq!(
        all.hits.iter().map(|h| h.session_id).collect::<Vec<_>>(),
        ids
    );
    assert_eq!((all.scanned, all.truncated), (3, false));

    let one = store.search_text(&query(1)).await.unwrap();
    assert_eq!(one.hits.len(), 1);
    assert_eq!((one.scanned, one.truncated), (1, true));

    let mut late = query(10);
    late.deadline = Some(std::time::Instant::now());
    let late = store.search_text(&late).await.unwrap();
    assert_eq!(
        (late.hits.len(), late.scanned, late.truncated),
        (0, 0, true)
    );
}

#[tokio::test]
async fn sidechains_are_passed_over_unless_asked_for() {
    let store = InMemoryHistoryStore::new();
    let side = SessionId::new();
    store.append(side, sidechain_meta()).await.unwrap();
    store
        .append_enveloped(
            side,
            vec![said(side, MessageOrigin::User, "deep in a sub-agent")],
        )
        .await
        .unwrap();
    let mut query = TextQuery {
        text: "sub-agent".into(),
        sessions: vec![side],
        include_sidechains: false,
        limit: 10,
        deadline: None,
        snippet_chars: 120,
    };
    assert!(store.search_text(&query).await.unwrap().hits.is_empty());
    query.include_sidechains = true;
    assert_eq!(store.search_text(&query).await.unwrap().hits.len(), 1);
}
