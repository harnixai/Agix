//! A PDF attached to a turn reaches the provider as a `document` block, and the ways it cannot do so are told to the model instead of failing the turn.

use std::time::Duration;

use crate::support::*;
use serde_json::{json, Value};

async fn turn_with_attachments(attachments: Value) -> Vec<Value> {
    let (srv, seen) = start_scripted_server(
        vec![text_round("seen")],
        ask_settings_no_memory(),
        Duration::ZERO,
    )
    .await;
    let request = json!({
        "jsonrpc": "2.0",
        "method": "session.run_turn",
        "params": { "message": "summarise the attachment", "attachments": attachments },
        "id": 1,
    });
    let (_frames, response) = rpc_streaming(&srv.sock, &request.to_string()).await;
    assert!(response["result"].is_object(), "turn failed: {response}");
    let first = seen.lock().unwrap()[0].clone();
    srv.stop().await;
    serde_json::to_value(&first).unwrap()["messages"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|m| m["content"].as_array().cloned().unwrap_or_default())
        .collect()
}

#[tokio::test]
async fn a_pdf_attachment_reaches_the_provider_as_a_document_block() {
    let blocks = turn_with_attachments(json!([{
        "kind": "document",
        "media_type": "application/pdf",
        "data": "JVBERi0xLjQgYm9keQ==",
        "name": "spec.pdf",
    }]))
    .await;
    let documents: Vec<&Value> = blocks.iter().filter(|b| b["type"] == "document").collect();
    assert_eq!(documents.len(), 1, "{blocks:?}");
    assert_eq!(
        documents[0]["source"],
        json!({"type": "base64", "media_type": "application/pdf", "data": "JVBERi0xLjQgYm9keQ=="})
    );
    assert_eq!(documents[0]["title"], "spec.pdf");
}

#[tokio::test]
async fn a_document_that_is_not_a_pdf_is_refused_in_text_and_the_turn_still_runs() {
    let blocks = turn_with_attachments(json!([{
        "kind": "document",
        "media_type": "application/msword",
        "data": "QUJD",
        "name": "old.doc",
    }]))
    .await;
    assert!(blocks.iter().all(|b| b["type"] != "document"), "{blocks:?}");
    let text: String = blocks
        .iter()
        .filter_map(|b| b["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("unsupported document type"), "{text}");
}

#[tokio::test]
async fn a_pdf_file_over_the_size_cap_is_refused_in_text_and_the_turn_still_runs() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("huge.pdf");
    std::fs::File::create(&path)
        .unwrap()
        .set_len(21 * 1024 * 1024)
        .unwrap();
    let blocks = turn_with_attachments(json!([{ "kind": "file", "path": path }])).await;
    assert!(blocks.iter().all(|b| b["type"] != "document"), "{blocks:?}");
    let text: String = blocks
        .iter()
        .filter_map(|b| b["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("document too large"), "{text}");
}
