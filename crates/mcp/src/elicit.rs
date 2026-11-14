//! An MCP server asking the user something in the middle of a tool call, and
//! how the question reaches the person behind that call. Only tool calls ask
//! anyone: a prompt or resource read that asks is answered `cancel`.
//!
//! A question arrives one of two ways. A 2026-07-28 server answers
//! `tools/call` with `input_required`; the call drives those rounds itself
//! ([`call_with_rounds`]) and asks through the call's [`McpCallScope`]. An
//! older server sends `elicitation/create` while the call is in flight; that
//! lands on the connection's handler, which cannot tell which call it belongs
//! to, since sessions share the connection. [`ElicitRoute`] hands it to the
//! call only when exactly one request is in flight on that connection, and
//! answers `cancel` otherwise: a guess could put one session's server's
//! question to another session's host.
//!
//! In the first case the tool time limit applies to each round, so the time a
//! person takes to answer does not count; in the second the server's
//! `tools/call` is still open while the person answers, and it does.

use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
pub use base::interface::elicitation::{McpElicitMode, McpElicitationAnswer};
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ElicitRequestParams, ElicitResult,
    ElicitationAction, InputRequest, InputRequests, InputResponses, DEFAULT_MRTR_MAX_ROUNDS,
};

use crate::error::McpError;

/// Who answers the questions one tool call's server asks.
#[async_trait]
pub trait McpElicitor: Send + Sync {
    async fn elicit(
        &self,
        server: &str,
        message: String,
        mode: McpElicitMode,
    ) -> McpElicitationAnswer;
}

/// What one `tools/call` carries down to the connection. With no elicitor,
/// every question is answered `cancel`.
#[derive(Clone, Default)]
pub struct McpCallScope {
    pub elicitor: Option<Arc<dyn McpElicitor>>,
}

type Asker = Option<Arc<dyn McpElicitor>>;

/// The requests in flight on one connection that a server's own
/// `elicitation/create` could belong to.
#[derive(Default)]
pub struct ElicitRoute {
    inflight: Mutex<Vec<(u64, Asker)>>,
    seq: AtomicU64,
}

/// One request entered in an [`ElicitRoute`], until dropped.
pub struct InFlight {
    route: Arc<ElicitRoute>,
    id: u64,
}

impl ElicitRoute {
    pub fn enter(self: &Arc<Self>, elicitor: Asker) -> InFlight {
        let id = self.seq.fetch_add(1, Ordering::Relaxed);
        self.lock().push((id, elicitor));
        InFlight {
            route: self.clone(),
            id,
        }
    }

    /// The elicitor of the one request in flight, or how many there are when
    /// that is not one.
    fn sole(&self) -> Result<Asker, usize> {
        match self.lock().as_slice() {
            [(_, elicitor)] => Ok(elicitor.clone()),
            all => Err(all.len()),
        }
    }

    /// Answer a server's own `elicitation/create`.
    pub(crate) async fn answer(&self, server: &str, params: ElicitRequestParams) -> ElicitResult {
        let answer = match self.sole() {
            Ok(elicitor) => elicit(server, params, elicitor.as_deref()).await,
            Err(inflight) => {
                tracing::warn!(
                    server,
                    inflight,
                    "an elicitation request could not be attributed to one tool call; cancelled"
                );
                McpElicitationAnswer::Cancel
            }
        };
        elicit_result(answer)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<(u64, Asker)>> {
        self.inflight.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        self.route.lock().retain(|(id, _)| *id != self.id);
    }
}

/// `tools/call` until the server completes it, answering each
/// `input_required` round through `scope`. `send` makes one request.
pub(crate) async fn call_with_rounds<F, Fut>(
    server: &str,
    mut params: CallToolRequestParams,
    scope: &McpCallScope,
    mut send: F,
) -> Result<CallToolResult, McpError>
where
    F: FnMut(CallToolRequestParams) -> Fut,
    Fut: Future<Output = Result<CallToolResponse, McpError>>,
{
    let mut state_only_rounds = 0u32;
    for _ in 0..DEFAULT_MRTR_MAX_ROUNDS {
        match send(params.clone()).await? {
            CallToolResponse::Complete(result) => return Ok(result),
            CallToolResponse::InputRequired(round) => {
                match round.input_requests.filter(|r| !r.is_empty()) {
                    Some(requests) => {
                        params.input_responses =
                            Some(answer_input_requests(server, requests, scope).await?);
                    }
                    None => {
                        params.input_responses = None;
                        let millis = (50u64 << state_only_rounds.min(3)).min(250);
                        tokio::time::sleep(Duration::from_millis(millis)).await;
                        state_only_rounds += 1;
                    }
                }
                params.request_state = round.request_state;
            }
            CallToolResponse::Task(_) => {
                return Err(McpError::RmcpService(format!(
                    "MCP server `{server}` answered the call with a task, which needs the tasks \
                     extension this client does not declare; it was not waited on"
                )))
            }
            _ => {
                return Err(McpError::RmcpService(format!(
                    "MCP server `{server}` answered the call with a result of an unknown type"
                )))
            }
        }
    }
    Err(McpError::RmcpService(format!(
        "MCP server `{server}` asked for input more than {DEFAULT_MRTR_MAX_ROUNDS} rounds"
    )))
}

/// One at a time, so a host is never shown two of a call's questions at once.
async fn answer_input_requests(
    server: &str,
    requests: InputRequests,
    scope: &McpCallScope,
) -> Result<InputResponses, McpError> {
    let mut responses = InputResponses::new();
    for (key, request) in requests {
        let InputRequest::Elicitation(request) = request else {
            return Err(McpError::RmcpService(format!(
                "MCP server `{server}` asked for sampling or roots, which this client does not \
                 offer"
            )));
        };
        let answer = elicit(server, request.params, scope.elicitor.as_deref()).await;
        responses.insert(key, serde_json::to_value(elicit_result(answer))?);
    }
    Ok(responses)
}

async fn elicit(
    server: &str,
    params: ElicitRequestParams,
    elicitor: Option<&dyn McpElicitor>,
) -> McpElicitationAnswer {
    let Some(elicitor) = elicitor else {
        return McpElicitationAnswer::Cancel;
    };
    let (message, mode) = match params {
        ElicitRequestParams::FormElicitationParams {
            message,
            requested_schema,
            ..
        } => match serde_json::to_value(requested_schema) {
            Ok(requested_schema) => (message, McpElicitMode::Form { requested_schema }),
            Err(_) => return McpElicitationAnswer::Decline,
        },
        ElicitRequestParams::UrlElicitationParams { message, url, .. } => {
            match openable_host(&url) {
                Some(host) => (message, McpElicitMode::Url { url, host }),
                None => {
                    tracing::warn!(server, "an elicitation URL that is not https was declined");
                    return McpElicitationAnswer::Decline;
                }
            }
        }
        _ => return McpElicitationAnswer::Decline,
    };
    elicitor.elicit(server, message, mode).await
}

/// The host of `url` when a person may be sent there: `https`, or `http` to
/// this machine.
fn openable_host(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    let host = parsed.host_str()?.to_string();
    let loopback = matches!(parsed.host(), Some(url::Host::Domain("localhost")))
        || match parsed.host() {
            Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
            Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
            _ => false,
        };
    match parsed.scheme() {
        "https" => Some(host),
        "http" if loopback => Some(host),
        _ => None,
    }
}

fn elicit_result(answer: McpElicitationAnswer) -> ElicitResult {
    match answer {
        McpElicitationAnswer::Accept { content } => {
            let result = ElicitResult::new(ElicitationAction::Accept);
            match content {
                Some(content) => result.with_content(content),
                None => result,
            }
        }
        McpElicitationAnswer::Decline => ElicitResult::new(ElicitationAction::Decline),
        McpElicitationAnswer::Cancel => ElicitResult::new(ElicitationAction::Cancel),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Says(McpElicitationAnswer);

    #[async_trait]
    impl McpElicitor for Says {
        async fn elicit(&self, _: &str, _: String, _: McpElicitMode) -> McpElicitationAnswer {
            self.0.clone()
        }
    }

    fn form() -> ElicitRequestParams {
        serde_json::from_value(serde_json::json!({
            "mode": "form",
            "message": "Pick a color",
            "requestedSchema": {
                "type": "object",
                "properties": { "color": { "type": "string", "enum": ["red", "blue"] } }
            }
        }))
        .unwrap()
    }

    fn url(url: &str) -> ElicitRequestParams {
        serde_json::from_value(serde_json::json!({
            "mode": "url", "message": "Sign in", "url": url, "elicitationId": "e1"
        }))
        .unwrap()
    }

    fn accepting() -> Arc<dyn McpElicitor> {
        Arc::new(Says(McpElicitationAnswer::Accept {
            content: Some(serde_json::json!({"color": "blue"})),
        }))
    }

    #[tokio::test]
    async fn a_lone_call_gets_its_servers_question_and_two_calls_get_none() {
        let route = Arc::new(ElicitRoute::default());
        let first = route.enter(Some(accepting()));
        let answered = route.answer("demo", form()).await;
        assert_eq!(answered.action, ElicitationAction::Accept);
        assert_eq!(answered.content, Some(serde_json::json!({"color": "blue"})));

        let second = route.enter(Some(accepting()));
        assert_eq!(
            route.answer("demo", form()).await.action,
            ElicitationAction::Cancel
        );
        drop((first, second));
        assert_eq!(
            route.answer("demo", form()).await.action,
            ElicitationAction::Cancel,
            "nothing in flight: nobody to ask"
        );
    }

    #[tokio::test]
    async fn only_an_https_or_loopback_url_is_put_to_anyone() {
        let elicitor = accepting();
        for (link, asked) in [
            ("https://auth.example.com/x", true),
            ("http://127.0.0.1:9/x", true),
            ("http://localhost/x", true),
            ("http://auth.example.com/x", false),
            ("javascript:alert(1)", false),
            ("not a url", false),
        ] {
            let answer = elicit("demo", url(link), Some(elicitor.as_ref())).await;
            assert_eq!(
                answer != McpElicitationAnswer::Decline,
                asked,
                "{link}: {answer:?}"
            );
        }
    }

    #[tokio::test]
    async fn rounds_are_driven_until_the_call_completes_and_a_task_is_refused() {
        use rmcp::model::{InputRequiredResult, Task, TaskStatus};
        let scope = McpCallScope {
            elicitor: Some(accepting()),
        };
        let mut sent: Vec<CallToolRequestParams> = Vec::new();
        let result = call_with_rounds("demo", CallToolRequestParams::new("ask"), &scope, |p| {
            let first = sent.is_empty();
            sent.push(p);
            async move {
                Ok(if first {
                    let request: InputRequest = serde_json::from_value(serde_json::json!({
                        "method": "elicitation/create",
                        "params": serde_json::to_value(form()).unwrap()
                    }))
                    .unwrap();
                    CallToolResponse::InputRequired(InputRequiredResult::new(
                        Some(InputRequests::from([("q".to_string(), request)])),
                        Some("state-1".into()),
                    ))
                } else {
                    CallToolResponse::Complete(CallToolResult::success(vec![]))
                })
            }
        })
        .await;
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[1].request_state.as_deref(), Some("state-1"));
        assert_eq!(
            sent[1].input_responses.as_ref().unwrap()["q"],
            serde_json::json!({"action": "accept", "content": {"color": "blue"}})
        );

        let task = call_with_rounds("demo", CallToolRequestParams::new("t"), &scope, |_| async {
            Ok(CallToolResponse::Task(rmcp::model::CreateTaskResult::new(
                Task::new("t1", TaskStatus::Working, "2026-01-01", "2026-01-01"),
            )))
        })
        .await;
        assert!(
            matches!(&task, Err(McpError::RmcpService(m)) if m.contains("tasks extension")),
            "{task:?}"
        );
    }
}
