//! 改会话规格与驱逐交错：改规格时内存里没有的规格从日志读回，不会写出一份只有这次改动的
//! 规格。

use super::permission_mode::{hold_session_lock, wait_for_lock_waiter, with_effort};
use super::rebuild::pool_with_history;
use super::*;
use crate::session_pool::api::SkillDirOption;

async fn session_with_effort(pool: &SessionPool) -> String {
    pool.create_session(None, ProjectSelector::Default, Some(&with_effort()))
        .await
        .expect("a session")
        .session_id
}

async fn assert_options_kept(pool: &SessionPool, sid: &str, what: &str) {
    let recorded = pool.specs.recorded(sid).await;
    assert!(
        recorded
            .options
            .as_ref()
            .is_some_and(|o| o.settings.is_some()),
        "{what} recorded a spec without the session's options: {:?}",
        recorded.options
    );
}

/// `change` runs against `sid` while the session leaves memory under it.
async fn change_as_the_session_leaves_memory<F, T>(
    pool: &Arc<SessionPool>,
    sid: &str,
    change: F,
) -> T
where
    F: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let (release, holder) = hold_session_lock(pool, sid).await;
    let changing = tokio::spawn(change);
    wait_for_lock_waiter(pool, sid).await;
    let live = pool.live.remove(sid).await.unwrap();
    pool.evict(sid, &live);
    release.send(()).unwrap();
    holder.await.unwrap();
    changing.await.unwrap()
}

#[tokio::test]
async fn skill_dirs_set_as_the_session_leaves_memory_keep_the_rest_of_its_spec() {
    let (pool, _dir, _store) = pool_with_history().await;
    let sid = session_with_effort(&pool).await;
    let dirs = vec![SkillDirOption {
        path: concat!(env!("CARGO_MANIFEST_DIR"), "/testdata").to_string(),
    }];
    let changing = {
        let pool = pool.clone();
        let sid = sid.clone();
        async move { pool.set_skill_dirs(&sid, dirs).await }
    };
    change_as_the_session_leaves_memory(&pool, &sid, changing)
        .await
        .expect("the change");
    assert_options_kept(&pool, &sid, "setSkillDirs").await;
    assert!(pool.specs.recorded(&sid).await.skill_dirs.is_some());
}

#[tokio::test]
async fn a_prompt_set_as_the_session_leaves_memory_keeps_the_rest_of_its_spec() {
    let (pool, _dir, _store) = pool_with_history().await;
    let sid = session_with_effort(&pool).await;
    let changing = {
        let pool = pool.clone();
        let sid = sid.clone();
        async move {
            pool.set_session_prompt(&sid, super::rebuild::appending("Briefly."))
                .await
                .map(|_| ())
        }
    };
    change_as_the_session_leaves_memory(&pool, &sid, changing)
        .await
        .expect("the change");
    assert_options_kept(&pool, &sid, "setPrompt").await;
    let recorded = pool.specs.recorded(&sid).await;
    assert_eq!(
        recorded.session_setting("prompt.append"),
        Some(&serde_json::json!("Briefly."))
    );
    assert_eq!(
        recorded.session_setting("model.effort"),
        Some(&serde_json::json!("high")),
        "the change starts from the settings the session was created with"
    );
}
