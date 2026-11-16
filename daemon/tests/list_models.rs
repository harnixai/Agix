//! `config.listModels` against stub endpoints, and a provider's per-model
//! `models` entries: written and read back in object form, and a model's
//! `max_output` holding the calls made to it.

use std::sync::{Arc, Mutex};

use model::mock::MockAnthropicClient;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const INVALID_PARAMS: i64 = -32602;
const ENGINE_ERROR: i64 = -32002;
const MODELS_UNSUPPORTED: i64 = -32027;

#[derive(Clone, Debug)]
struct Seen {
    path: String,
    headers: Vec<(String, String)>,
    body: Value,
}

impl Seen {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

/// An HTTP endpoint that answers each request with what `route` makes of its
/// path, and remembers every request.
struct Stub {
    base: String,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl Stub {
    async fn start(route: fn(&str) -> String) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let seen: Arc<Mutex<Vec<Seen>>> = Arc::default();
        let record = seen.clone();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let record = record.clone();
                tokio::spawn(async move {
                    let Some(request) = read_request(&mut sock).await else {
                        return;
                    };
                    let response = route(&request.path);
                    record.lock().unwrap().push(request);
                    let _ = sock.write_all(response.as_bytes()).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        Self { base, seen }
    }

    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
}

async fn read_request(sock: &mut tokio::net::TcpStream) -> Option<Seen> {
    let mut buf = Vec::new();
    let head_end = loop {
        if let Some(at) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break at + 4;
        }
        let mut chunk = [0u8; 4096];
        let n = sock.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut lines = head.lines();
    let path = lines.next()?.split_whitespace().nth(1)?.to_string();
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    let len: usize = headers
        .iter()
        .find(|(k, _)| k == "content-length")
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    let mut body = buf[head_end..].to_vec();
    while body.len() < len {
        let mut chunk = [0u8; 4096];
        let n = sock.read(&mut chunk).await.ok()?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    Some(Seen {
        path,
        headers,
        body: serde_json::from_slice(&body).unwrap_or(Value::Null),
    })
}

fn http(code: u16, body: &str) -> String {
    format!(
        "HTTP/1.1 {code} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
         connection: close\r\n\r\n{body}",
        body.len()
    )
}

fn sse_text(text: &str) -> String {
    let events = [
        json!({"type": "message_start", "message": {"id": "msg", "model": "m", "role": "assistant",
            "usage": {"input_tokens": 3, "output_tokens": 0}}}),
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": text}}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 1}}),
        json!({"type": "message_stop"}),
    ];
    let mut out = String::from(
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n",
    );
    for e in events {
        out.push_str(&format!(
            "event: {}\ndata: {e}\n\n",
            e["type"].as_str().unwrap()
        ));
    }
    out
}

/// Anthropic's `/v1/models` in two pages, OpenAI's `/v1/models`, and a
/// conversation endpoint that says one word.
fn catalog_route(path: &str) -> String {
    if path.starts_with("/anthropic/v1/models") {
        let page = if path.contains("after_id=claude-b") {
            json!({"data": [{"type": "model", "id": "claude-c", "display_name": "Claude C"}],
                   "has_more": false, "first_id": "claude-c", "last_id": "claude-c"})
        } else {
            json!({"data": [
                       {"type": "model", "id": "claude-a", "display_name": "Claude A"},
                       {"type": "model", "id": "claude-b", "display_name": "Claude B"}],
                   "has_more": true, "first_id": "claude-a", "last_id": "claude-b"})
        };
        return http(200, &page.to_string());
    }
    if path.starts_with("/openai/v1/models") {
        let body = json!({"object": "list", "data": [
            {"id": "gpt-x", "object": "model", "owned_by": "x"},
            {"id": "vendor/r", "object": "model", "context_length": 65536}]});
        return http(200, &body.to_string());
    }
    if path.ends_with("/v1/messages") {
        return sse_text("ok");
    }
    http(500, r#"{"error":{"message":"unexpected path"}}"#)
}

fn not_found_route(_: &str) -> String {
    http(404, r#"{"error":{"message":"Not Found"}}"#)
}

fn provider(api_type: &str, base_url: String, key: &str, models: Value) -> Value {
    json!({"api_type": api_type, "base_url": base_url, "api_key": key,
           "default_model": "claude-a", "models": models})
}

async fn daemon_with(providers: Value) -> crate::support::Served {
    let providers: std::collections::HashMap<String, base::provider::ProviderConfig> =
        serde_json::from_value(providers).unwrap();
    let default = providers.keys().min().cloned();
    crate::support::serve_permitting_all(
        Arc::new(MockAnthropicClient::new()),
        Default::default(),
        |config| {
            config.settings.values.providers = providers;
            config.settings.values.default_provider = default;
        },
    )
    .await
}

#[tokio::test]
async fn list_models_answers_what_each_endpoint_lists_asked_with_the_configured_key() {
    let stub = Stub::start(catalog_route).await;
    let served = daemon_with(json!({
        "anth": provider("anthropic", format!("{}/anthropic", stub.base), "k-anth", json!([])),
        "oai": provider("openai_compatible", format!("{}/openai/v1", stub.base), "k-oai", json!([])),
    }))
    .await;

    let listed = crate::support::call(
        &served.sock,
        "config.listModels",
        json!({"provider": "anth"}),
    )
    .await;
    assert_eq!(
        listed["models"],
        json!([
            {"id": "claude-a", "display_name": "Claude A"},
            {"id": "claude-b", "display_name": "Claude B"},
            {"id": "claude-c", "display_name": "Claude C"},
        ]),
        "{listed}"
    );
    let anthropic: Vec<Seen> = stub
        .seen()
        .into_iter()
        .filter(|s| s.path.starts_with("/anthropic/v1/models"))
        .collect();
    assert_eq!(
        anthropic.len(),
        2,
        "both pages were asked for: {anthropic:?}"
    );
    for request in &anthropic {
        assert_eq!(request.header("x-api-key"), Some("k-anth"));
        assert!(request.header("anthropic-version").is_some(), "{request:?}");
    }

    let listed = crate::support::call(
        &served.sock,
        "config.listModels",
        json!({"provider": "oai"}),
    )
    .await;
    assert_eq!(
        listed["models"],
        json!([{"id": "gpt-x"}, {"id": "vendor/r", "context_window": 65536}]),
        "{listed}"
    );
    let openai = stub
        .seen()
        .into_iter()
        .find(|s| s.path == "/openai/v1/models")
        .expect("the OpenAI-compatible endpoint was asked");
    assert_eq!(openai.header("authorization"), Some("Bearer k-oai"));

    let unknown = crate::support::call_frame(
        &served.sock,
        "config.listModels",
        json!({"provider": "nope"}),
    )
    .await;
    assert_eq!(unknown["error"]["code"], INVALID_PARAMS, "{unknown}");
}

#[tokio::test]
async fn an_endpoint_that_does_not_list_models_is_models_unsupported() {
    let stub = Stub::start(not_found_route).await;
    let served = daemon_with(json!({
        "gw": provider("openai_compatible", format!("{}/v1", stub.base), "k", json!([])),
    }))
    .await;
    let refused =
        crate::support::call_frame(&served.sock, "config.listModels", json!({"provider": "gw"}))
            .await;
    assert_eq!(refused["error"]["code"], MODELS_UNSUPPORTED, "{refused}");
    assert!(
        refused["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("404")),
        "{refused}"
    );
}

#[tokio::test]
async fn a_provider_without_a_key_is_not_asked_and_reads_back_its_models_as_written() {
    let stub = Stub::start(catalog_route).await;
    let served = daemon_with(json!({})).await;
    let models = json!([
        "plain",
        {"id": "rich", "context_window": 1000, "max_output": 100,
         "reasoning_levels": ["low", "high"], "input_types": ["text", "image"]},
    ]);
    crate::support::call(
        &served.sock,
        "config.setProvider",
        json!({"tier": "global", "provider_id": "keyless", "config": {
            "api_type": "openai_compatible", "base_url": format!("{}/openai/v1", stub.base),
            "api_key_env": "AGIX_TEST_76_NO_SUCH_KEY", "models": models}}),
    )
    .await;

    let view = crate::support::call(&served.sock, "config.getProvider", json!({})).await;
    assert_eq!(view["providers"]["keyless"]["models"], models, "{view}");

    let refused = crate::support::call_frame(
        &served.sock,
        "config.listModels",
        json!({"provider": "keyless"}),
    )
    .await;
    assert_eq!(refused["error"]["code"], ENGINE_ERROR, "{refused}");
    assert_eq!(
        refused["error"]["data"]["kind"], "no_credentials",
        "{refused}"
    );
    assert!(stub.seen().is_empty(), "{:?}", stub.seen());

    let bad = crate::support::call_frame(
        &served.sock,
        "config.setProvider",
        json!({"tier": "global", "provider_id": "keyless", "config": {"models": [{"max_output": 1}]}}),
    )
    .await;
    assert_eq!(bad["error"]["code"], INVALID_PARAMS, "{bad}");
}

#[tokio::test]
async fn a_models_max_output_holds_the_calls_made_to_it() {
    let stub = Stub::start(catalog_route).await;
    let served = daemon_with(json!({
        "anth": provider("anthropic", format!("{}/anthropic", stub.base), "k",
                         json!([{"id": "claude-a", "max_output": 100}])),
    }))
    .await;
    let session = crate::support::call(
        &served.sock,
        "session.create",
        json!({"options": {"provider": "anth"}}),
    )
    .await["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    let turn = crate::support::call_frame(
        &served.sock,
        "session.run_turn",
        json!({"session_id": session, "message": "say ok"}),
    )
    .await;
    assert!(turn.get("result").is_some(), "{turn}");

    let calls: Vec<Seen> = stub
        .seen()
        .into_iter()
        .filter(|s| s.path.ends_with("/v1/messages") && s.body.get("tools").is_some())
        .collect();
    assert!(
        !calls.is_empty(),
        "the turn reached the stub: {:?}",
        stub.seen()
    );
    for call in calls {
        assert_eq!(call.body["model"], "claude-a");
        assert_eq!(call.body["max_tokens"], 100);
    }
}

/// A listing that always has one more page, each named after the one before.
fn endless_route(path: &str) -> String {
    let after: u32 = path
        .split("after_id=m")
        .nth(1)
        .and_then(|n| n.split('&').next()?.parse().ok())
        .unwrap_or(0);
    let id = format!("m{}", after + 1);
    let page = json!({"data": [{"type": "model", "id": id}], "has_more": true,
                      "first_id": id, "last_id": id});
    http(200, &page.to_string())
}

/// One page of models larger than any listing has reason to be.
fn huge_route(_: &str) -> String {
    let filler = "x".repeat(9 * 1024 * 1024);
    let page = json!({"data": [{"id": "big", "description": filler}], "has_more": false});
    http(200, &page.to_string())
}

#[tokio::test]
async fn a_listing_cut_off_at_its_page_limit_says_so() {
    let stub = Stub::start(endless_route).await;
    let served = daemon_with(json!({
        "anth": provider("anthropic", stub.base.clone(), "k", json!([])),
    }))
    .await;
    let listed = crate::support::call(
        &served.sock,
        "config.listModels",
        json!({"provider": "anth"}),
    )
    .await;
    assert_eq!(listed["truncated"], true, "{listed}");
    assert_eq!(listed["models"].as_array().unwrap().len(), 50, "{listed}");

    let short = Stub::start(catalog_route).await;
    let served = daemon_with(json!({
        "anth": provider("anthropic", format!("{}/anthropic", short.base), "k", json!([])),
    }))
    .await;
    let listed = crate::support::call(
        &served.sock,
        "config.listModels",
        json!({"provider": "anth"}),
    )
    .await;
    assert_eq!(listed["truncated"], false, "{listed}");
}

#[tokio::test]
async fn a_listing_larger_than_the_cap_is_refused_rather_than_read_whole() {
    let stub = Stub::start(huge_route).await;
    let served = daemon_with(json!({
        "oai": provider("openai_compatible", format!("{}/v1", stub.base), "k", json!([])),
    }))
    .await;
    let refused = crate::support::call_frame(
        &served.sock,
        "config.listModels",
        json!({"provider": "oai"}),
    )
    .await;
    assert_eq!(refused["error"]["code"], ENGINE_ERROR, "{refused}");
}
