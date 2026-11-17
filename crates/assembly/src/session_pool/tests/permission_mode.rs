//! `session.setPermissionMode` 在会话池里：与驱逐交错时不丢规格的其余部分、写不进日志时
//! 报 `persist_failed`、记下的是夹紧后的模式。

use super::mcp_for_project::test_pool_with_store;
use super::rebuild::{new_session, pool_with_history, slot_and_state, StoreThatGoesDown};
use super::*;
use base::context::Session;
use base::permission::PermissionMode;
use std::sync::atomic::Ordering;

fn with_effort() -> SessionOptions {
    SessionOptions {
        effort: Some(base::interface::model::Effort::High),
        ..Default::default()
    }
}

/// Holds `sid`'s session lock until the returned sender is used.
async fn hold_session_lock(
    pool: &Arc<SessionPool>,
    sid: &str,
) -> (
    tokio::sync::oneshot::Sender<()>,
    tokio::task::JoinHandle<()>,
) {
    let (entered_tx, entered) = tokio::sync::oneshot::channel();
    let (release, released) = tokio::sync::oneshot::channel::<()>();
    let holder = {
        let pool = pool.clone();
        let sid = sid.to_string();
        tokio::spawn(async move {
            pool.live
                .with_session_lock(&sid, async {
                    let _ = entered_tx.send(());
                    let _ = released.await;
                })
                .await
        })
    };
    entered.await.unwrap();
    (release, holder)
}

async fn wait_for_lock_waiter(pool: &SessionPool, sid: &str) {
    for _ in 0..500 {
        if pool.live.session_lock_holders(sid).await >= 3 {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    panic!("nothing came to wait on the session lock");
}

/// The session leaves memory while the switch waits for its lock: the mode
/// lands in the spec the log recorded, which keeps everything else it had.
#[tokio::test]
async fn a_mode_set_as_the_session_leaves_memory_keeps_the_rest_of_its_spec() {
    let (pool, _dir, _store) = pool_with_history().await;
    let sid = pool
        .create_session(None, ProjectSelector::Default, Some(&with_effort()))
        .await
        .expect("a session")
        .session_id;
    let (release, holder) = hold_session_lock(&pool, &sid).await;
    let switching = {
        let pool = pool.clone();
        let sid = sid.clone();
        tokio::spawn(async move {
            pool.set_permission_mode(&sid, PermissionMode::DontAsk)
                .await
        })
    };
    wait_for_lock_waiter(&pool, &sid).await;
    let live = pool.live.remove(&sid).await.unwrap();
    pool.evict(&sid, &live);
    release.send(()).unwrap();
    holder.await.unwrap();

    let choice = switching.await.unwrap().expect("the switch");
    assert_eq!(choice.mode, PermissionMode::DontAsk);
    let recorded = pool.specs.recorded(&sid).await;
    assert!(
        recorded
            .options
            .as_ref()
            .is_some_and(|o| o.effort.is_some()),
        "the switch recorded a spec without the session's options: {:?}",
        recorded.options
    );
    assert_eq!(recorded.permission_mode, Some(PermissionMode::DontAsk));
}

fn persist_failed<T: std::fmt::Debug>(result: Result<T, PoolError>, what: &str) {
    match result {
        Err(PoolError::Engine(failure)) => {
            assert_eq!(
                failure.kind,
                base::event::FailureKind::PersistFailed,
                "{what}"
            )
        }
        other => panic!("{what}: {other:?}"),
    }
}

/// A switch the log does not take is not reported as done: in memory and
/// out of it, to a mode the spec records and into plan, which the session
/// state records.
#[tokio::test]
async fn a_switch_the_log_does_not_take_is_refused_as_persist_failed() {
    let store = Arc::new(StoreThatGoesDown::default());
    let (pool, _dir) = test_pool_with_store(Some(store.clone())).await;
    let sid = new_session(&pool).await;
    let (_, state) = slot_and_state(&pool, &sid).await;

    store.down.store(true, Ordering::SeqCst);
    persist_failed(
        pool.set_permission_mode(&sid, PermissionMode::DontAsk)
            .await,
        "a live session's spec",
    );
    assert_eq!(state.permission_mode(), PermissionMode::Default);
    assert_eq!(
        pool.specs.get(&sid).and_then(|spec| spec.permission_mode),
        None
    );
    persist_failed(
        pool.set_permission_mode(&sid, PermissionMode::Plan).await,
        "a live session's plan state",
    );

    store.down.store(false, Ordering::SeqCst);
    pool.set_permission_mode(&sid, PermissionMode::Default)
        .await
        .expect("the log takes it again");
    let live = pool.live.remove(&sid).await.unwrap();
    pool.evict(&sid, &live);
    store.down.store(true, Ordering::SeqCst);
    persist_failed(
        pool.set_permission_mode(&sid, PermissionMode::DontAsk)
            .await,
        "a recorded session's spec",
    );
    persist_failed(
        pool.set_permission_mode(&sid, PermissionMode::Plan).await,
        "a recorded session's plan state",
    );
}

/// What the spec records is the mode the session got, not the wider one
/// asked for: settings loosened later do not widen it unasked.
#[tokio::test]
async fn the_spec_records_the_mode_granted_not_the_one_asked_for() {
    let (pool, _dir, _store) = pool_with_history().await;
    let sid = new_session(&pool).await;
    let choice = pool
        .set_permission_mode(&sid, PermissionMode::BypassPermissions)
        .await
        .expect("the switch");
    assert!(choice.clamped);
    assert_eq!(
        pool.specs.get(&sid).and_then(|spec| spec.permission_mode),
        Some(PermissionMode::Default)
    );
    assert_eq!(
        pool.specs.recorded(&sid).await.permission_mode,
        Some(PermissionMode::Default)
    );
}
