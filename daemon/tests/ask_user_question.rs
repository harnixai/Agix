//! `AskUserQuestion` between the model and the host: offered only to hosts that answer, declined, timed out, answered late.

use std::time::Duration;

use crate::support::*;
use daemon::rpc::codes;
use model::stream::StreamEvent;

const ANSWERS_QUESTIONS: &str = r#","options":{"prompt_types":["permission","question"]}"#;

fn asking_a_question_rounds() -> Vec<Vec<StreamEvent>> {
    vec![
        tool_round(
            "AskUserQuestion",
            "tool-1",
            serde_json::json!({
                "question": "which branch?",
                "header": "Branch",
                "options": [{"key": "a", "label": "main"}, {"key": "b", "label": "dev"}],
            }),
        ),
        text_round("thanks"),
    ]
}

/// Answer a question with `answer`, and permit anything else that asks.
fn answering_questions_with(answer: &str) -> impl FnMut(&serde_json::Value) -> Option<String> {
    let answer = answer.to_string();
    move |event| {
        Some(if event["prompt_type"] == "question" {
            answer.clone()
        } else {
            r#"{"type":"permit"}"#.to_string()
        })
    }
}

/// What the model was sent back for its `AskUserQuestion` call, as the next
/// request carried it.
fn sent_back(seen: &SeenRequests) -> String {
    let requests = seen.lock().unwrap();
    requests
        .iter()
        .map(|r| format!("{:?}", r.messages))
        .find(|m| m.contains("tool-1") && m.contains("ToolResult"))
        .unwrap_or_else(|| panic!("no request carried the question's result"))
}

fn offered_tools(seen: &SeenRequests) -> Vec<String> {
    let requests = seen.lock().unwrap();
    requests
        .first()
        .expect("the turn called the model")
        .tools
        .iter()
        .map(|t| t.name.clone())
        .collect()
}

#[tokio::test]
async fn a_session_that_answers_questions_puts_the_question_to_the_host_and_the_answer_to_the_model(
) {
    let (srv, seen) = start_scripted_server(
        asking_a_question_rounds(),
        ask_settings_no_memory(),
        Duration::from_secs(30),
    )
    .await;

    let (frames, response) = run_turn_answering_prompts(
        &srv.sock,
        "ask me",
        ANSWERS_QUESTIONS,
        answering_questions_with(r#"{"type":"answer","text":"a"}"#),
    )
    .await;
    assert!(response["result"].is_object(), "turn failed: {response}");

    let questions: Vec<_> = events_of_kind(&frames, "prompt")
        .into_iter()
        .filter(|p| p["prompt_type"] == "question")
        .collect();
    assert_eq!(questions.len(), 1, "{frames:?}");
    assert_eq!(
        *questions[0],
        serde_json::json!({
            "kind": "prompt", "prompt_type": "question",
            "prompt_id": "tool-1", "tool_use_id": "tool-1",
            "question": "which branch?", "header": "Branch",
            "options": [{"key": "a", "label": "main"}, {"key": "b", "label": "dev"}],
        })
    );
    assert!(offered_tools(&seen).iter().any(|t| t == "AskUserQuestion"));
    let result = sent_back(&seen);
    assert!(result.contains("a: main"), "{result}");

    srv.stop().await;
}

#[tokio::test]
async fn a_session_that_did_not_say_it_answers_questions_is_not_offered_ask_user_question() {
    let (srv, seen) = start_scripted_server(
        vec![text_round("hi")],
        ask_settings_no_memory(),
        Duration::from_secs(30),
    )
    .await;

    run_turn(&srv.sock, None, "hello").await;

    let tools = offered_tools(&seen);
    assert!(
        !tools.iter().any(|t| t == "AskUserQuestion"),
        "a tool every call of which is declined must not be offered: {tools:?}"
    );
    assert!(tools.iter().any(|t| t == "ExitPlanMode"), "{tools:?}");

    srv.stop().await;
}

#[tokio::test]
async fn a_declined_question_reaches_the_model_as_unanswered() {
    let (srv, seen) = start_scripted_server(
        asking_a_question_rounds(),
        ask_settings_no_memory(),
        Duration::from_secs(30),
    )
    .await;

    let (_frames, response) = run_turn_answering_prompts(
        &srv.sock,
        "ask me",
        ANSWERS_QUESTIONS,
        answering_questions_with(r#"{"type":"decline","reason":"ask me later"}"#),
    )
    .await;
    assert!(response["result"].is_object(), "turn failed: {response}");

    let result = sent_back(&seen);
    assert!(
        result.contains("has not answered") && result.contains("ask me later"),
        "{result}"
    );

    srv.stop().await;
}

#[tokio::test]
async fn a_question_nobody_answers_times_out_and_the_model_is_told() {
    let (srv, seen) = start_scripted_server(
        asking_a_question_rounds(),
        ask_settings_no_memory(),
        Duration::from_secs(1),
    )
    .await;

    let (frames, response) = tokio::time::timeout(
        Duration::from_secs(20),
        run_turn_answering_prompts(&srv.sock, "ask me", ANSWERS_QUESTIONS, |event| {
            (event["prompt_type"] != "question").then(|| r#"{"type":"permit"}"#.to_string())
        }),
    )
    .await
    .expect("an unanswered question must not hold the turn open");
    assert!(response["result"].is_object(), "turn failed: {response}");
    assert!(!events_of_kind(&frames, "prompt").is_empty(), "{frames:?}");

    let result = sent_back(&seen);
    assert!(
        result.contains("has not answered") && result.contains("within 1s"),
        "{result}"
    );

    srv.stop().await;
}

/// Start a turn in a session that answers questions and wait until its
/// question is open, as `session.subscribe` hands it to a late subscriber.
async fn open_question(
    sock: &std::path::Path,
) -> (
    String,
    serde_json::Value,
    tokio::task::JoinHandle<(Vec<serde_json::Value>, serde_json::Value)>,
) {
    let created = rpc(
        sock,
        r#"{"jsonrpc":"2.0","method":"session.create","id":1,"params":{"options":{"prompt_types":["permission","question"]}}}"#,
    )
    .await;
    let sid = created["result"]["session_id"]
        .as_str()
        .unwrap_or_else(|| panic!("{created}"))
        .to_string();
    let turn = {
        let (sock, sid) = (sock.to_path_buf(), sid.clone());
        tokio::spawn(async move {
            rpc_streaming(
                &sock,
                &format!(
                    r#"{{"jsonrpc":"2.0","method":"session.run_turn","params":{{"session_id":"{sid}","message":"ask me"}},"id":1}}"#
                ),
            )
            .await
        })
    };
    for _ in 0..200 {
        let subscribed = rpc(
            sock,
            &format!(
                r#"{{"jsonrpc":"2.0","method":"session.subscribe","id":2,"params":{{"session_id":"{sid}"}}}}"#
            ),
        )
        .await;
        if let Some(prompt) = subscribed["result"]["pending_prompts"]
            .as_array()
            .and_then(|p| p.first())
        {
            return (sid, prompt.clone(), turn);
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("the question never opened");
}

async fn respond(sock: &std::path::Path, sid: &str, rest: &str) -> serde_json::Value {
    rpc(
        sock,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"session.respondToPrompt","id":3,"params":{{"session_id":"{sid}","prompt_id":"tool-1",{rest}}}}}"#
        ),
    )
    .await
}

#[tokio::test]
async fn an_open_question_is_handed_to_a_late_subscriber_and_can_be_answered_from_there() {
    let (srv, seen) = start_scripted_server(
        asking_a_question_rounds(),
        ask_settings_no_memory(),
        Duration::from_secs(30),
    )
    .await;

    let (sid, prompt, turn) = open_question(&srv.sock).await;
    let event = &prompt["params"]["event"];
    assert_eq!(event["kind"], "prompt", "{prompt}");
    assert_eq!(event["prompt_type"], "question", "{prompt}");
    assert_eq!(event["prompt_id"], "tool-1", "{prompt}");
    assert_eq!(event["question"], "which branch?", "{prompt}");

    let ack = respond(
        &srv.sock,
        &sid,
        r#""prompt_type":"question","answer":{"type":"answer","text":"the release branch"}"#,
    )
    .await;
    assert_eq!(ack["result"]["prompt_id"], "tool-1", "{ack}");
    let (_frames, response) = turn.await.unwrap();
    assert!(response["result"].is_object(), "turn failed: {response}");
    let result = sent_back(&seen);
    assert!(result.contains("the release branch"), "{result}");

    srv.stop().await;
}

#[tokio::test]
async fn a_question_answered_as_a_permission_prompt_is_refused() {
    let (srv, seen) = start_scripted_server(
        asking_a_question_rounds(),
        ask_settings_no_memory(),
        Duration::from_secs(30),
    )
    .await;

    let (sid, _prompt, turn) = open_question(&srv.sock).await;
    for wrong in [
        r#""decision":{"type":"permit"}"#,
        r#""prompt_type":"permission","decision":{"type":"permit"}"#,
    ] {
        let refused = respond(&srv.sock, &sid, wrong).await;
        assert_eq!(refused["error"]["code"], codes::INVALID_PARAMS, "{refused}");
    }

    let ack = respond(
        &srv.sock,
        &sid,
        r#""prompt_type":"question","answer":{"type":"answer","text":"b"}"#,
    )
    .await;
    assert_eq!(ack["result"]["prompt_id"], "tool-1", "{ack}");
    let (_frames, response) = turn.await.unwrap();
    assert!(response["result"].is_object(), "turn failed: {response}");
    let result = sent_back(&seen);
    assert!(result.contains("b: dev"), "{result}");

    srv.stop().await;
}
