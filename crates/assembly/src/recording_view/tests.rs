use super::*;
use base::interface::model::{
    MessageRole, ModelContentBlock, ModelError, ModelMessage, ThinkingMode, ToolDef,
};
use serde_json::json;
use telemetry::recorder::format::{
    CallRecord, ChunkRecord, EndRecord, Header, Record, RecordedError, RecordedParams,
    FORMAT_VERSION,
};
use telemetry::recorder::writer::RecordingWriter;

const T0: u64 = 1_763_200_000_000;

fn header() -> Header {
    Header {
        version: FORMAT_VERSION,
        name: "rec".into(),
        session_id: "S0".into(),
        parent: None,
        agent_type: None,
        created_at: T0,
        engine_version: "test".into(),
    }
}

fn call(seq: u64, ts: u64, session: &str, blobs: &BlobStore) -> CallRecord {
    let system = vec![
        blobs
            .put(&PromptBlock {
                role: BlockRole::System,
                content: "You are Agix.".into(),
                cache_strategy: Some(CacheStrategy::Ephemeral),
                name: Some("identity".into()),
                origin: BlockOrigin::Kernel,
            })
            .unwrap(),
        blobs
            .put(&PromptBlock {
                role: BlockRole::System,
                content: "x".repeat(70 * 1024),
                cache_strategy: None,
                name: Some("plugin.big".into()),
                origin: BlockOrigin::Plugin("p1".into()),
            })
            .unwrap(),
    ];
    let tools = blobs
        .put(&vec![ToolDef::new(
            "Read",
            "Reads a file.",
            json!({"type": "object"}),
        )])
        .unwrap();
    let messages = vec![blobs
        .put(&ModelMessage {
            role: MessageRole::User,
            content: vec![
                ModelContentBlock::Text {
                    text: "what is in this picture?".into(),
                },
                ModelContentBlock::Image {
                    media_type: "image/png".into(),
                    data: "aGVsbG8=".into(),
                },
            ],
        })
        .unwrap()];
    CallRecord {
        seq,
        ts,
        session_id: Some(session.into()),
        parent_session_id: None,
        agent_type: None,
        turn_id: None,
        turn: 1,
        step: 0,
        purpose: None,
        provider: "anthropic".into(),
        api_type: base::provider::ApiType::Anthropic,
        params: RecordedParams {
            model: "claude-test".into(),
            max_tokens: 8192,
            thinking_mode: ThinkingMode::Off,
            effort: None,
            fallback_model: None,
            cache_edits: vec![],
            search_max_uses: None,
        },
        system,
        tools,
        messages,
        input_map: None,
    }
}

fn chunk(writer: &RecordingWriter, call: u64, ts: u64, chunk: ModelEvent) {
    writer
        .append(&Record::Chunk(ChunkRecord {
            seq: writer.next_seq(),
            ts,
            call,
            chunk,
        }))
        .unwrap();
}

fn end(writer: &RecordingWriter, call: u64, ts: u64, outcome: Outcome, usage: Option<Usage>) {
    writer
        .append(&Record::End(EndRecord {
            seq: writer.next_seq(),
            ts,
            call,
            outcome,
            stop_reason: usage.map(|_| "tool_use".to_string()),
            usage,
            duration_ms: ts - T0,
        }))
        .unwrap();
}

/// A parent call that used a tool, a sub-agent's call that failed, and a
/// naming call still in flight.
fn recording() -> (tempfile::TempDir, Opened) {
    let root = tempfile::tempdir().unwrap();
    let writer = RecordingWriter::create(root.path(), header());
    let blobs = writer.blobs().clone();

    let mut parent = call(writer.next_seq(), T0, "S0", &blobs);
    parent.turn_id = Some("T1".into());
    writer.append(&Record::Call(Box::new(parent))).unwrap();
    chunk(
        &writer,
        0,
        T0 + 800,
        ModelEvent::TextDelta { text: "Hel".into() },
    );
    chunk(
        &writer,
        0,
        T0 + 810,
        ModelEvent::TextDelta { text: "lo".into() },
    );
    chunk(
        &writer,
        0,
        T0 + 820,
        ModelEvent::ThinkingDelta { text: "hmm".into() },
    );
    chunk(
        &writer,
        0,
        T0 + 900,
        ModelEvent::ToolUse {
            id: "tu1".into(),
            name: "Read".into(),
            input: json!({"path": "/x"}),
        },
    );
    end(
        &writer,
        0,
        T0 + 4200,
        Outcome::Ok,
        Some(Usage {
            input_tokens: 10,
            output_tokens: 20,
            cache_creation_input_tokens: 30,
            cache_read_input_tokens: 40,
        }),
    );

    let child_seq = writer.next_seq();
    let mut child = call(child_seq, T0 + 5000, "S1", &blobs);
    child.parent_session_id = Some("S0".into());
    child.agent_type = Some("explore".into());
    writer.append(&Record::Call(Box::new(child))).unwrap();
    end(
        &writer,
        child_seq,
        T0 + 5100,
        Outcome::Error {
            error: RecordedError::from(&ModelError::overloaded().with_status(529)),
        },
        None,
    );

    let title_seq = writer.next_seq();
    let mut title = call(title_seq, T0 + 6000, "S0", &blobs);
    title.purpose = Some("title".into());
    writer.append(&Record::Call(Box::new(title))).unwrap();
    writer.flush().unwrap();

    let opened = open(&root.path().join("rec")).unwrap().unwrap();
    (root, opened)
}

#[test]
fn the_call_rows_are_the_protocol_s_shape() {
    let (_root, opened) = recording();
    assert_eq!(opened.recording.calls.len(), 3);
    let (rows, next) = opened.summaries(0, 10, true);
    assert_eq!(next, 2, "the call still streaming is read again");
    let system = json!([
        {"source": "identity", "chars": 13, "digest": "879106ac742f0567"},
        {"source": "plugin.big", "chars": 70 * 1024, "digest": "c4de74a820aee087"},
    ]);
    let input = json!({"system": system, "tools": 1, "tools_digest": "286e970158096479",
                       "messages": 1});
    let no_output = json!({"chars": 0, "text_preview": "", "thinking_chars": 0, "tool_uses": []});
    assert_eq!(
        serde_json::to_value(&rows).unwrap(),
        json!([
            {
                "index": 0, "session_id": "S0", "parent_session_id": null, "agent_type": null,
                "turn_id": "T1", "turn": 1, "step": 0, "purpose": null,
                "ts": "2025-11-15T09:46:40Z", "provider": "anthropic", "model": "claude-test",
                "input": input,
                "output": {"chars": 5, "text_preview": "Hello", "thinking_chars": 3,
                           "tool_uses": [{"id": "tu1", "name": "Read",
                                          "input_preview": "{\"path\":\"/x\"}"}]},
                "status": "ok", "stop_reason": "tool_use",
                "usage": {"input_tokens": 10, "output_tokens": 20,
                          "cache_creation_input_tokens": 30, "cache_read_input_tokens": 40},
                "timing": {"sent_at": "2025-11-15T09:46:40Z", "ttft_ms": 800, "duration_ms": 4200},
                "error": null
            },
            {
                "index": 1, "session_id": "S1", "parent_session_id": "S0", "agent_type": "explore",
                "turn_id": null, "turn": 1, "step": 0, "purpose": null,
                "ts": "2025-11-15T09:46:45Z", "provider": "anthropic", "model": "claude-test",
                "input": input, "output": no_output,
                "status": "error", "stop_reason": null, "usage": null,
                "timing": {"sent_at": "2025-11-15T09:46:45Z", "ttft_ms": null, "duration_ms": 5100},
                "error": {"kind": "overloaded", "message": "", "http_status": 529,
                          "retry_after_ms": null}
            },
            {
                "index": 2, "session_id": "S0", "parent_session_id": null, "agent_type": null,
                "turn_id": null, "turn": 1, "step": 0, "purpose": "title",
                "ts": "2025-11-15T09:46:46Z", "provider": "anthropic", "model": "claude-test",
                "input": input, "output": no_output,
                "status": "in_flight", "stop_reason": null, "usage": null,
                "timing": {"sent_at": "2025-11-15T09:46:46Z", "ttft_ms": null, "duration_ms": null},
                "error": null
            }
        ])
    );
    assert_eq!(
        serde_json::to_value(opened.info()).unwrap(),
        json!({"name": "rec", "created_at": "2025-11-15T09:46:40Z", "covers": "latest_run",
               "generation": T0})
    );
}

#[test]
fn a_call_no_process_is_still_writing_is_unfinished_and_does_not_hold_next_back() {
    let (_root, opened) = recording();
    let (rows, next) = opened.summaries(1, 10, false);
    assert_eq!(next, 3);
    assert_eq!(rows[1].status, CallStatus::Unfinished);
    let (rows, next) = opened.summaries(0, 1, true);
    assert_eq!((rows.len(), next), (1, 1));
    let (rows, next) = opened.summaries(7, 10, true);
    assert_eq!((rows.len(), next), (0, 3));
}

#[test]
fn a_call_in_full_inlines_what_fits_and_refers_to_the_rest() {
    let (_root, opened) = recording();
    let detail = serde_json::to_value(opened.detail("S0", 0, true).unwrap()).unwrap();
    let big = &detail["request"]["system"][1];
    let handle = big["blob"].as_str().unwrap().to_string();
    assert!(handle.starts_with("s.") && handle.len() == 18, "{big}");
    assert_eq!(
        detail,
        json!({
            "session_id": "S0", "index": 0, "generation": T0,
            "request": {
                "session_id": "S0", "parent_session_id": null, "agent_type": null,
                "turn_id": "T1", "turn": 1, "step": 0, "purpose": null,
                "provider": "anthropic", "model": "claude-test", "max_tokens": 8192,
                "thinking_mode": "off", "effort": null, "fallback_model": null,
                "system": [
                    {"role": "system", "cache": "ephemeral", "source": "identity",
                     "origin": "kernel", "text": "You are Agix."},
                    {"blob": handle, "bytes": big["bytes"]}
                ],
                "tools": [{"name": "Read", "description": "Reads a file.",
                           "input_schema": {"type": "object"}}],
                "messages": [{"role": "user", "content": [
                    {"type": "text", "text": "what is in this picture?"},
                    {"type": "image", "media_type": "image/png", "bytes": 5}
                ]}],
                "input_map": null
            },
            "response": {
                "status": "ok", "text": "Hello", "thinking_chars": 3,
                "tool_uses": [{"id": "tu1", "name": "Read", "input": {"path": "/x"}}],
                "stop_reason": "tool_use",
                "usage": {"input_tokens": 10, "output_tokens": 20,
                          "cache_creation_input_tokens": 30, "cache_read_input_tokens": 40},
                "ttft_ms": 800, "duration_ms": 4200, "error": null
            }
        })
    );

    let blob = opened.blob(&handle).unwrap();
    assert_eq!(blob.value["origin"], "plugin:p1");
    assert_eq!(blob.value["text"].as_str().unwrap().len(), 70 * 1024);
    assert_eq!(
        blob.bytes,
        serde_json::to_string(&blob.value).unwrap().len()
    );
    assert_eq!(Some(blob.bytes), big["bytes"].as_u64().map(|b| b as usize));
    assert!(opened.detail("S0", 3, true).is_none());
}

#[test]
fn a_blob_reference_is_only_ever_a_kind_and_sixteen_hex_digits() {
    let (_root, opened) = recording();
    for bad in [
        "",
        "s.",
        "x.0123456789abcdef",
        "s.0123456789ABCDEF",
        "s.0123456789abcde",
        "s.0123456789abcdef0",
        "s.../../calls.json",
        "m./etc/passwd",
        "s.0123456789abcdef/",
    ] {
        assert_eq!(
            opened.blob(bad).unwrap_err(),
            BlobLookup::Malformed,
            "{bad:?}"
        );
    }
    assert_eq!(
        opened.blob("m.0123456789abcdef").unwrap_err(),
        BlobLookup::Absent
    );
}

#[test]
fn nothing_written_yet_reads_as_no_recording() {
    let root = tempfile::tempdir().unwrap();
    assert!(open(&root.path().join("absent")).unwrap().is_none());
}

fn tool_result_message(blobs: &BlobStore, id: &str, is_error: bool) -> BlobId {
    blobs
        .put(&ModelMessage {
            role: MessageRole::User,
            content: vec![ModelContentBlock::ToolResult {
                tool_use_id: id.into(),
                content: "out".into(),
                is_error: Some(is_error),
            }],
        })
        .unwrap()
}

#[test]
fn a_row_quotes_the_answer_and_the_arguments_and_finds_each_result_in_the_next_call() {
    let root = tempfile::tempdir().unwrap();
    let writer = RecordingWriter::create(root.path(), header());
    let blobs = writer.blobs().clone();
    let long_text = "答".repeat(250);
    let long_arg = "x".repeat(300);

    let first_seq = writer.next_seq();
    let first = call(first_seq, T0, "S0", &blobs);
    writer.append(&Record::Call(Box::new(first))).unwrap();
    chunk(
        &writer,
        first_seq,
        T0 + 1,
        ModelEvent::TextDelta {
            text: long_text.clone(),
        },
    );
    for (id, input) in [("ok1", json!({"a": long_arg})), ("bad1", json!({"b": 1}))] {
        chunk(
            &writer,
            first_seq,
            T0 + 2,
            ModelEvent::ToolUse {
                id: id.into(),
                name: "Bash".into(),
                input,
            },
        );
    }
    end(&writer, first_seq, T0 + 10, Outcome::Ok, None);

    let aux_seq = writer.next_seq();
    let mut aux = call(aux_seq, T0 + 20, "S0", &blobs);
    aux.purpose = Some("title".into());
    aux.messages = vec![tool_result_message(&blobs, "ok1", true)];
    writer.append(&Record::Call(Box::new(aux))).unwrap();
    end(&writer, aux_seq, T0 + 30, Outcome::Ok, None);

    let other_seq = writer.next_seq();
    let mut other = call(other_seq, T0 + 40, "S1", &blobs);
    other.messages = vec![tool_result_message(&blobs, "ok1", true)];
    writer.append(&Record::Call(Box::new(other))).unwrap();
    end(&writer, other_seq, T0 + 50, Outcome::Ok, None);

    let second_seq = writer.next_seq();
    let mut second = call(second_seq, T0 + 60, "S0", &blobs);
    second.messages = vec![
        tool_result_message(&blobs, "old", true),
        assistant_tool_use(&blobs, &["ok1", "bad1"]),
        tool_result_message(&blobs, "ok1", false),
        tool_result_message(&blobs, "bad1", true),
    ];
    writer.append(&Record::Call(Box::new(second))).unwrap();
    end(&writer, second_seq, T0 + 70, Outcome::Ok, None);
    writer.flush().unwrap();

    let opened = open(&root.path().join("rec")).unwrap().unwrap();
    let (rows, _) = opened.summaries(0, 10, false);
    let first = &rows[0].output;
    assert_eq!(first.text_preview, format!("{}…", "答".repeat(200)));
    assert_eq!(first.chars, 250);
    let uses = &first.tool_uses;
    assert_eq!(uses[0].input_preview.chars().count(), 201);
    assert!(uses[0].input_preview.starts_with("{\"a\":\"xx"));
    assert_eq!(uses[1].input_preview, "{\"b\":1}");
    assert_eq!(uses[0].result_is_error, Some(false));
    assert_eq!(uses[1].result_is_error, Some(true));
    let json = serde_json::to_value(&rows[3].output).unwrap();
    assert_eq!(json["tool_uses"], json!([]));
}

#[test]
fn a_result_with_no_next_conversation_call_is_left_out() {
    let (_root, opened) = recording();
    let (rows, _) = opened.summaries(0, 10, false);
    assert_eq!(rows[0].output.tool_uses[0].result_is_error, None);
    let json = serde_json::to_value(&rows[0].output.tool_uses[0]).unwrap();
    assert!(json.get("result_is_error").is_none(), "{json}");
}

fn assistant_tool_use(blobs: &BlobStore, ids: &[&str]) -> BlobId {
    blobs
        .put(&ModelMessage {
            role: MessageRole::Assistant,
            content: ids
                .iter()
                .map(|id| ModelContentBlock::ToolUse {
                    id: (*id).into(),
                    name: "Bash".into(),
                    input: json!({}),
                })
                .collect(),
        })
        .unwrap()
}

/// A provider that numbers tool calls per reply (`call_0`) repeats ids from
/// step to step. A call that streamed a tool use and then failed is retried
/// with the same messages, so the next conversation call's trailing results
/// are the *previous* step's — they must not be read as this call's.
#[test]
fn a_failed_calls_tool_use_does_not_take_the_previous_steps_result() {
    let root = tempfile::tempdir().unwrap();
    let writer = RecordingWriter::create(root.path(), header());
    let blobs = writer.blobs().clone();
    let use_call_0 = |_seq: u64| ModelEvent::ToolUse {
        id: "call_0".into(),
        name: "Bash".into(),
        input: json!({}),
    };

    let a_seq = writer.next_seq();
    let a = call(a_seq, T0, "S0", &blobs);
    let prompt = a.messages.clone();
    writer.append(&Record::Call(Box::new(a))).unwrap();
    chunk(&writer, a_seq, T0 + 1, use_call_0(a_seq));
    end(&writer, a_seq, T0 + 2, Outcome::Ok, None);

    let mut step_two = prompt.clone();
    step_two.push(assistant_tool_use(&blobs, &["call_0"]));
    step_two.push(tool_result_message(&blobs, "call_0", true));

    let b_seq = writer.next_seq();
    let mut b = call(b_seq, T0 + 10, "S0", &blobs);
    b.messages = step_two.clone();
    writer.append(&Record::Call(Box::new(b))).unwrap();
    chunk(&writer, b_seq, T0 + 11, use_call_0(b_seq));
    end(
        &writer,
        b_seq,
        T0 + 12,
        Outcome::Error {
            error: RecordedError::from(&ModelError::overloaded()),
        },
        None,
    );

    let c_seq = writer.next_seq();
    let mut c = call(c_seq, T0 + 20, "S0", &blobs);
    c.messages = step_two;
    writer.append(&Record::Call(Box::new(c))).unwrap();
    end(&writer, c_seq, T0 + 21, Outcome::Ok, None);
    writer.flush().unwrap();

    let opened = open(&root.path().join("rec")).unwrap().unwrap();
    let (rows, _) = opened.summaries(0, 10, false);
    assert_eq!(rows[0].output.tool_uses[0].result_is_error, Some(true));
    assert_eq!(
        rows[1].output.tool_uses[0].result_is_error, None,
        "the failed call's call_0 never ran; its flag came from the step before"
    );
}
