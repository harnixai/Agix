//! An engine that dies under a turn: the caller is told the engine stopped,
//! watchers of daemon events see the session closed for that reason, and the
//! session is not lost — the next call on its id brings it back from its
//! transcript.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::support::text_round;
use daemon::config::{load_daemon_config, StaticDaemonPaths};
use daemon::DaemonServer;
use model::client::{AnthropicClient, CountFuture, EventStream};
use model::types::MessagesRequest;
use rpc_client::DaemonRpcClient;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

struct DiesOnTheFirstCall(AtomicBool);

impl AnthropicClient for DiesOnTheFirstCall {
    fn stream_messages(&self, _req: MessagesRequest) -> EventStream {
        if !self.0.swap(true, Ordering::SeqCst) {
            panic!("the model client fell over");
        }
        Box::pin(futures::stream::iter(
            text_round("back again").into_iter().map(Ok),
        ))
    }

    fn count_tokens<'a>(&'a self, _req: &'a MessagesRequest) -> CountFuture<'a> {
        Box::pin(async { Ok(0usize) })
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_engine_that_dies_mid_turn_closes_its_session_as_engine_stopped_and_it_resumes(
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().to_path_buf();
    let sock = root.join("exit.sock");
    crate::support::trust_projects_under(&root, &root);
    std::fs::write(
        root.join("settings.json"),
        json!({ "memory": {"enabled": false} }).to_string(),
    )?;
    let config = load_daemon_config(
        "claude-sonnet-4-6",
        2000,
        Some(&sock),
        "code",
        &StaticDaemonPaths::new(root.clone()),
    );
    let store: Arc<dyn history::store::HistoryStore> = Arc::new(
        history::store::JsonlHistoryStore::with_roots(
            &root,
            history::path::HistoryRoots::under(&root),
        )
        .await?,
    );
    let pool = daemon::assemble::pool(
        &config,
        Arc::new(scene::code::spec(scene::code::Interaction::Interactive)),
        daemon::Assembly {
            cwd: Some(root.clone()),
            model_client: Some(Arc::new(DiesOnTheFirstCall(AtomicBool::new(false)))),
            transcripts: daemon::Transcripts::In(store),
            ..Default::default()
        },
    )
    .await?;
    let server = Arc::new(DaemonServer::new(pool, CancellationToken::new()));
    let serving = server.clone();
    let bind = sock.clone();
    let handle = tokio::spawn(async move {
        let _ = serving.serve_unix(&bind).await;
    });
    for _ in 0..80 {
        if sock.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    let mut watcher = DaemonRpcClient::connect(&sock).await?;
    let subscribed = watcher.call("daemon.subscribeEvents", Value::Null).await?;
    anyhow::ensure!(subscribed.error.is_none(), "{subscribed:?}");

    let mut client = DaemonRpcClient::connect(&sock).await?;
    let sid = client.session_create(json!({})).await?;
    let died = tokio::time::timeout(
        Duration::from_secs(20),
        client.call(
            "session.run_turn",
            json!({ "session_id": sid, "message": "first" }),
        ),
    )
    .await
    .map_err(|_| anyhow::anyhow!("a turn whose engine died never ended"))??;
    let error = died.error.expect("the turn fails when its engine dies");
    assert_eq!(
        error.data.clone().unwrap_or_default()["kind"],
        "engine_stopped",
        "{error:?}"
    );

    let closed = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let Some(frame) = watcher.next_frame().await? else {
                anyhow::bail!("the watcher's connection closed");
            };
            if frame["method"] == "daemon.event"
                && frame["params"]["kind"] == "session_closed"
                && frame["params"]["session_id"] == sid.as_str()
            {
                return anyhow::Ok(frame["params"].clone());
            }
        }
    })
    .await??;
    assert_eq!(closed["reason"], "engine_stopped", "{closed}");

    let again = client
        .call(
            "session.run_turn",
            json!({ "session_id": sid, "message": "second" }),
        )
        .await?;
    assert!(
        again.error.is_none(),
        "the session did not come back after its engine died: {again:?}"
    );
    server.shutdown_token().cancel();
    let _ = handle.await;
    Ok(())
}

struct DiesOnTheFirstCallAfterAWhile(AtomicBool);

impl AnthropicClient for DiesOnTheFirstCallAfterAWhile {
    fn stream_messages(&self, _req: MessagesRequest) -> EventStream {
        if !self.0.swap(true, Ordering::SeqCst) {
            return Box::pin(futures::stream::once(async {
                tokio::time::sleep(Duration::from_millis(500)).await;
                panic!("the model client fell over");
            }));
        }
        Box::pin(futures::stream::iter(
            text_round("back again").into_iter().map(Ok),
        ))
    }

    fn count_tokens<'a>(&'a self, _req: &'a MessagesRequest) -> CountFuture<'a> {
        Box::pin(async { Ok(0usize) })
    }
}

/// A turn queued behind the one whose engine dies runs on the session brought
/// back from its transcript, as the next call would.
#[tokio::test(flavor = "multi_thread")]
async fn a_turn_queued_behind_an_engine_that_dies_runs_on_the_session_resumed() -> anyhow::Result<()>
{
    let dir = tempfile::tempdir()?;
    let root = dir.path().to_path_buf();
    let sock = root.join("exit.sock");
    crate::support::trust_projects_under(&root, &root);
    std::fs::write(
        root.join("settings.json"),
        json!({ "memory": {"enabled": false} }).to_string(),
    )?;
    let config = load_daemon_config(
        "claude-sonnet-4-6",
        2000,
        Some(&sock),
        "code",
        &StaticDaemonPaths::new(root.clone()),
    );
    let store: Arc<dyn history::store::HistoryStore> = Arc::new(
        history::store::JsonlHistoryStore::with_roots(
            &root,
            history::path::HistoryRoots::under(&root),
        )
        .await?,
    );
    let pool = daemon::assemble::pool(
        &config,
        Arc::new(scene::code::spec(scene::code::Interaction::Interactive)),
        daemon::Assembly {
            cwd: Some(root.clone()),
            model_client: Some(Arc::new(DiesOnTheFirstCallAfterAWhile(AtomicBool::new(
                false,
            )))),
            transcripts: daemon::Transcripts::In(store),
            ..Default::default()
        },
    )
    .await?;
    let server = Arc::new(DaemonServer::new(pool, CancellationToken::new()));
    let serving = server.clone();
    let bind = sock.clone();
    let handle = tokio::spawn(async move {
        let _ = serving.serve_unix(&bind).await;
    });
    for _ in 0..80 {
        if sock.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    let mut first = DaemonRpcClient::connect(&sock).await?;
    let mut second = DaemonRpcClient::connect(&sock).await?;
    let sid = first.session_create(json!({})).await?;
    let dying = first.call(
        "session.run_turn",
        json!({ "session_id": sid, "message": "first" }),
    );
    let queued = async {
        tokio::time::sleep(Duration::from_millis(150)).await;
        second
            .call(
                "session.run_turn",
                json!({ "session_id": sid, "message": "second", "on_busy": "queue" }),
            )
            .await
    };
    let (died, queued) = tokio::time::timeout(Duration::from_secs(30), async {
        tokio::join!(dying, queued)
    })
    .await
    .map_err(|_| anyhow::anyhow!("the turns never ended"))?;
    assert!(
        died?.error.is_some(),
        "the first turn fails with its engine"
    );
    let queued = queued?;
    assert!(
        queued.error.is_none(),
        "the queued turn did not run on the resumed session: {queued:?}"
    );
    server.shutdown_token().cancel();
    let _ = handle.await;
    Ok(())
}
