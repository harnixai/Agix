//! `eviction_tests` 原样搬来的用例。

use super::*;
use crate::session_pool::parts::live::{
    evict_lru, idle_session_ids, EngineSlot, LiveSessions, Naming,
};
use crate::session_pool::parts::phase::Generations;
use base::context::Session;
use runtime::agent::InputSender;

/// A `LiveSession` with no real agent behind it — only `last_active`
/// (and `cancel`, so eviction's `.cancel()` call doesn't panic) matter
/// for these tests.
fn dummy_live_session(last_active: Instant) -> LiveSession {
    let (input_tx, _input_rx): (InputSender, _) = tokio::sync::mpsc::unbounded_channel();
    let (_event_tx, event_rx): (_, EventReceiver) = tokio::sync::mpsc::unbounded_channel();
    LiveSession {
        core: SessionCore::active_at(
            "dummy",
            Arc::new(scene::code::spec(scene::code::Interaction::Interactive)),
            None.into(),
            Naming::default(),
            last_active,
        ),
        engine: EngineSlot {
            generation: Generations::default().next(),
            input_tx,
            cancel: CancellationToken::new(),
            end_reason: Default::default(),
            exited: CancellationToken::new(),
            namer: SessionNamer::inert("dummy"),
            binding: crate::session_pool::parts::bindings::BindingStamp {
                key: (None, String::new()),
                epoch: 0,
            },
            delegator: None,
            scripts: None,
            health: Default::default(),
            commands: Default::default(),
        },
        turn: TurnSlot::new(event_rx),
    }
}

#[test]
fn evict_lru_removes_the_least_recently_active_session() {
    let now = Instant::now();
    let mut sessions = HashMap::new();
    sessions.insert(
        "oldest".to_string(),
        dummy_live_session(now.checked_sub(Duration::from_secs(60)).unwrap()),
    );
    sessions.insert(
        "middle".to_string(),
        dummy_live_session(now.checked_sub(Duration::from_secs(30)).unwrap()),
    );
    sessions.insert("newest".to_string(), dummy_live_session(now));

    evict_lru(&mut sessions, |_| false);

    assert_eq!(
        sessions.len(),
        2,
        "sessions: {:?}",
        sessions.keys().collect::<Vec<_>>()
    );
    assert!(!sessions.contains_key("oldest"));
    assert!(sessions.contains_key("middle"));
    assert!(sessions.contains_key("newest"));
}

#[test]
fn inserting_over_an_existing_sid_must_cancel_the_replaced_sessions_token() {
    // Mirrors the pattern `create()` uses: `sessions.insert(sid, new_live)`
    // returning `Some(old)` means a live entry already existed under that
    // exact sid — reachable via the config-generation recreate path in
    // `run_turn`. `CancellationToken` isn't tied to `Drop`, so failing to
    // call `.cancel()` on the returned old value would leak the old
    // Agent's background `tokio::spawn` loop forever. This test isolates
    // just that HashMap-replace-then-cancel pattern (not the full
    // `create()`/`SessionPool`, which needs a live client/settings/etc.).
    let mut sessions = HashMap::new();
    let old_session = dummy_live_session(Instant::now());
    let old_cancel = old_session.engine.cancel.clone();
    sessions.insert("sid".to_string(), old_session);
    assert!(!old_cancel.is_cancelled());

    let new_session = dummy_live_session(Instant::now());
    if let Some(old) = sessions.insert("sid".to_string(), new_session) {
        old.engine.cancel.cancel();
    }

    assert!(
        old_cancel.is_cancelled(),
        "replaced session's token must be cancelled"
    );
    assert_eq!(sessions.len(), 1);
}

fn run_a_task(live: &LiveSession) {
    live.core.state().register_task(
        "task-1".into(),
        base::context::TaskType::Agent,
        CancellationToken::new(),
        serde_json::json!({}),
    );
}

#[test]
fn eviction_passes_over_a_session_that_is_doing_something() {
    let now = Instant::now();
    let long_ago = now.checked_sub(Duration::from_secs(600)).unwrap();
    let mut sessions = HashMap::new();
    let mut in_a_turn = dummy_live_session(long_ago);
    in_a_turn
        .turn
        .claim(RunningTurn::new("t1".into(), Initiator::Client, None))
        .unwrap();
    sessions.insert("in_a_turn".to_string(), in_a_turn);
    let mut rebuilding = dummy_live_session(long_ago);
    rebuilding.turn.begin_rebuild().unwrap();
    sessions.insert("rebuilding".to_string(), rebuilding);
    let with_a_task = dummy_live_session(long_ago);
    run_a_task(&with_a_task);
    sessions.insert("with_a_task".to_string(), with_a_task);

    assert!(idle_session_ids(&sessions, now, Duration::from_secs(300)).is_empty());
    assert!(
        evict_lru(&mut sessions, |_| false).is_none(),
        "every session is busy"
    );
    assert_eq!(sessions.len(), 3);

    sessions.insert("idle".to_string(), dummy_live_session(now));
    assert_eq!(
        evict_lru(&mut sessions, |_| false)
            .map(|(sid, _)| sid)
            .as_deref(),
        Some("idle")
    );
}

#[test]
fn eviction_passes_over_a_session_whose_log_lacks_what_it_holds() {
    let now = Instant::now();
    let long_ago = now.checked_sub(Duration::from_secs(600)).unwrap();
    let mut sessions = HashMap::new();
    let behind = dummy_live_session(long_ago);
    behind.core.log().set_behind(true);
    sessions.insert("behind".to_string(), behind);

    assert!(idle_session_ids(&sessions, now, Duration::from_secs(300)).is_empty());
    assert!(evict_lru(&mut sessions, |_| false).is_none());

    sessions["behind"].core.log().set_behind(false);
    assert_eq!(
        idle_session_ids(&sessions, now, Duration::from_secs(300)),
        ["behind"]
    );
}

#[test]
fn evict_lru_on_empty_map_is_a_noop() {
    let mut sessions: HashMap<String, LiveSession> = HashMap::new();
    evict_lru(&mut sessions, |_| false); // must not panic
    assert!(sessions.is_empty());
}

#[test]
fn idle_session_ids_selects_only_sessions_past_timeout() {
    let now = Instant::now();
    let mut sessions = HashMap::new();
    sessions.insert(
        "stale".to_string(),
        dummy_live_session(now.checked_sub(Duration::from_secs(600)).unwrap()),
    );
    sessions.insert("fresh".to_string(), dummy_live_session(now));

    let idle = idle_session_ids(&sessions, now, Duration::from_secs(300));
    assert_eq!(idle, vec!["stale".to_string()]);
}

#[test]
fn idle_session_ids_empty_when_nothing_past_timeout() {
    let now = Instant::now();
    let mut sessions = HashMap::new();
    sessions.insert("fresh".to_string(), dummy_live_session(now));

    let idle = idle_session_ids(&sessions, now, Duration::from_secs(300));
    assert!(idle.is_empty());
}

#[test]
fn idle_session_ids_exactly_at_timeout_boundary_is_not_evicted() {
    // 边界用 `>` 不是 `>=`（见 idle_session_ids 实现）——正好等于超时
    // 阈值的 session 这一轮还不驱逐，下一轮才会（因为到那时肯定 `>` 了）。
    let now = Instant::now();
    let mut sessions = HashMap::new();
    sessions.insert(
        "exactly_at_boundary".to_string(),
        dummy_live_session(now.checked_sub(Duration::from_secs(300)).unwrap()),
    );

    let idle = idle_session_ids(&sessions, now, Duration::from_secs(300));
    assert!(idle.is_empty(), "idle: {idle:?}");
}

/// Whatever holds a session's lock is changing it: neither a full table nor
/// the janitor takes it out of memory meanwhile.
#[tokio::test]
async fn a_session_whose_lock_is_held_is_not_evicted() {
    let now = Instant::now();
    let long_ago = now.checked_sub(Duration::from_secs(600)).unwrap();
    let live = Arc::new(LiveSessions::new(1, Duration::from_secs(300)));
    live.insert_evicting("held", dummy_live_session(long_ago), |_| true)
        .await
        .unwrap();
    let (entered_tx, entered) = tokio::sync::oneshot::channel();
    let (release, released) = tokio::sync::oneshot::channel::<()>();
    let holder = {
        let live = live.clone();
        tokio::spawn(async move {
            live.with_session_lock("held", async {
                let _ = entered_tx.send(());
                let _ = released.await;
            })
            .await
        })
    };
    entered.await.unwrap();

    let displaced = live
        .insert_evicting("next", dummy_live_session(now), |_| true)
        .await
        .unwrap();
    assert!(
        displaced.evicted.is_none(),
        "a full table evicted a session being changed"
    );
    let a_day_later = now + Duration::from_secs(24 * 3600);
    let (taken, _) = live.take_idle(a_day_later).await;
    let taken: Vec<String> = taken.into_iter().map(|(sid, _)| sid).collect();
    assert_eq!(
        taken,
        ["next"],
        "the janitor evicted a session being changed"
    );

    release.send(()).unwrap();
    holder.await.unwrap();
    let (taken, _) = live.take_idle(a_day_later).await;
    let taken: Vec<String> = taken.into_iter().map(|(sid, _)| sid).collect();
    assert_eq!(taken, ["held"]);
}
