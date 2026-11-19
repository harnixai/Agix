//! A project whose provider speaks OpenAI Responses: one turn with a tool
//! round, seen from the wire — the request is Responses-shaped, the tool
//! result returns as a `function_call_output`, and the turn reports usage.

use blackbox::{LiveConfig, LiveRun, ProviderStub, Reply};
use serde_json::json;

#[tokio::test(flavor = "multi_thread")]
async fn a_responses_provider_runs_a_tool_round_and_reports_usage() -> anyhow::Result<()> {
    let stub = ProviderStub::start().await?;
    let mut run = LiveRun::builder("responses-round")
        .config(LiveConfig::for_endpoint(
            &stub.base_url(),
            blackbox::ModelEndpoint::model(&stub),
        ))
        .settings(json!({
            "providers": [{
                "id": "resp",
                "vendor": "openai",
                "api": "openai_responses",
                "base_url": stub.base_url().as_str(),
                "api_key_env": "ANTHROPIC_AUTH_TOKEN",
                "tiers": {"strong": "responses-stub-model"}
            }]
        }))
        .start()
        .await?;
    stub.script_when("标题生成器", (0..4).map(|_| Reply::responses_text("title")));
    let session = run.session().await?;
    run.write_file("note.txt", "RESPONSES-FILE-BODY\n")?;
    let path = run.project.join("note.txt").to_string_lossy().to_string();
    stub.script([
        Reply::responses_calls("call_resp_1", "Read", json!({ "file_path": path })),
        Reply::responses_text("RESPONSES-DONE"),
    ]);

    let turn = run.turn(&session, "read the note").await?;
    assert!(turn.text.contains("RESPONSES-DONE"), "{}", turn.text);
    let (_, output, is_error) = turn
        .tool_results
        .iter()
        .find(|(name, _, _)| name == "Read")
        .expect("Read result");
    assert!(
        !is_error && output.contains("RESPONSES-FILE-BODY"),
        "{output}"
    );

    let calls: Vec<_> = stub
        .seen()
        .into_iter()
        .filter(|r| r.path.ends_with("/responses"))
        .filter(|r| r.body["tools"].as_array().is_some_and(|t| !t.is_empty()))
        .collect();
    assert_eq!(calls.len(), 2, "{calls:?}");
    let first = &calls[0].body;
    assert_eq!(first["stream"], true);
    assert!(
        first["input"].is_array() && first.get("messages").is_none(),
        "{first}"
    );
    assert!(
        first["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["type"] == "function" && t["name"] == "Read"),
        "{first}"
    );
    let second = calls[1].body["input"].as_array().unwrap();
    assert!(
        second
            .iter()
            .any(|i| i["type"] == "function_call" && i["call_id"] == "call_resp_1"),
        "{second:?}"
    );
    let output = second
        .iter()
        .find(|i| i["type"] == "function_call_output" && i["call_id"] == "call_resp_1")
        .expect("the tool result went back");
    assert!(
        output["output"].to_string().contains("RESPONSES-FILE-BODY"),
        "{output}"
    );

    let done = turn.turn_complete_event.expect("turn_complete");
    assert_eq!(done["stop_reason"], "end_turn", "{done}");
    assert_eq!(
        done["usage"]["input_tokens"],
        2 * blackbox::provider::INPUT_TOKENS,
        "{done}"
    );
    assert_eq!(
        done["usage"]["output_tokens"],
        2 * blackbox::provider::OUTPUT_TOKENS,
        "{done}"
    );
    assert_eq!(
        done["usage"]["cache_read_input_tokens"],
        2 * blackbox::provider::CACHE_READ_TOKENS,
        "{done}"
    );
    run.finish().await;
    Ok(())
}
