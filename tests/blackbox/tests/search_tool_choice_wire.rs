//! A `WebSearch` call goes to the provider as a call that is only a search.
//! Anthropic's API refuses a forced `tool_choice` from Opus and Sonnet 5.5,
//! Fable and Mythos 5.1 and later, so on those the search is asked for in
//! the prompt; older models are still made to search.

use blackbox::{Block, LiveConfig, LiveRun, ProviderStub, Reply, SeenRequest};
use serde_json::{json, Value};

const QUERY: &str = "SEARCH-CHOICE-QUERY";

struct Searched {
    search: SeenRequest,
    result: String,
    failed: bool,
}

/// One turn of a session on `model` that calls `WebSearch`, its search call
/// answered with `search_reply`.
async fn search_on(model: &str, search_reply: Reply) -> anyhow::Result<Searched> {
    let stub = ProviderStub::start().await?;
    let mut run = LiveRun::builder("search-choice")
        .config(LiveConfig::for_endpoint(&stub.base_url(), model))
        .start()
        .await?;
    let session = run.session().await?;
    let _host = run.watch(&session, true).await?;
    stub.script_when(QUERY, [search_reply]);
    stub.script([
        Reply::calls("s1", "WebSearch", json!({ "query": QUERY })),
        Reply::text("done"),
    ]);
    let events = run.turn(&session, "search the web").await?;
    let (_, result, failed) = events
        .tool_results
        .iter()
        .find(|(name, _, _)| name == "WebSearch")
        .cloned()
        .expect("a WebSearch result");
    let search = stub
        .calls()
        .into_iter()
        .find(is_provider_search)
        .expect("a search call");
    run.finish().await;
    Ok(Searched {
        search,
        result,
        failed,
    })
}

fn is_provider_search(call: &SeenRequest) -> bool {
    call.body["tools"].as_array().is_some_and(|tools| {
        tools.iter().any(|t| {
            t["type"]
                .as_str()
                .is_some_and(|t| t.starts_with("web_search_"))
        })
    })
}

fn found() -> Reply {
    Reply::Blocks(vec![
        Block::searched(&[("Found page", "https://found.example/page")]),
        Block::text("FOUND-MARK"),
    ])
}

#[tokio::test(flavor = "multi_thread")]
async fn on_sonnet_5_5_the_search_is_asked_for_not_forced() -> anyhow::Result<()> {
    let searched = search_on("claude-sonnet-5-5", found()).await?;
    let choice = searched.search.body["tool_choice"]["type"].clone();
    assert!(
        choice == json!("auto") || choice == Value::Null,
        "tool_choice: {}",
        searched.search.body
    );
    assert!(
        searched.search.system_contains("web_search"),
        "the prompt names the tool: {}",
        searched.search.system_text()
    );
    assert!(!searched.failed, "{}", searched.result);
    assert!(
        searched.result.contains("https://found.example/page"),
        "{}",
        searched.result
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn on_sonnet_5_the_search_is_still_forced() -> anyhow::Result<()> {
    let searched = search_on("claude-sonnet-5", found()).await?;
    assert_eq!(
        searched.search.body["tool_choice"],
        json!({ "type": "tool", "name": "web_search" }),
        "{}",
        searched.search.body
    );
    assert!(!searched.failed, "{}", searched.result);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_search_call_answered_without_searching_says_so() -> anyhow::Result<()> {
    let searched = search_on("claude-sonnet-5-5", Reply::text("ANSWER-FROM-MEMORY")).await?;
    assert!(searched.failed, "{}", searched.result);
    assert!(
        searched.result.contains("without searching the web")
            && searched.result.contains("ANSWER-FROM-MEMORY"),
        "{}",
        searched.result
    );
    Ok(())
}
