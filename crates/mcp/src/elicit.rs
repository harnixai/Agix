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
//! One call has at most [`MAX_OPEN_PER_CALL`] questions of an older server
//! open at once and [`MAX_QUESTIONS_PER_CALL`] in all, and
//! [`MAX_QUESTIONS_PER_ROUND`] in a round; the rest are answered `cancel`. A question's words are bounded too: a longer message is
//! cut, a larger form is declined.
//!
//! In the first case the tool time limit applies to each round, so the time a
//! person takes to answer does not count; in the second the server's
//! `tools/call` is still open while the person answers, and it does.

use std::future::Future;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
pub use base::interface::elicitation::{McpElicitMode, McpElicitationAnswer};
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ElicitRequestParams, ElicitResult,
    ElicitationAction, InputRequest, InputRequests, InputResponses, DEFAULT_MRTR_MAX_ROUNDS,
};

use crate::error::McpError;
use tokio_util::sync::CancellationToken;

pub const MAX_OPEN_PER_CALL: usize = 4;
pub const MAX_QUESTIONS_PER_ROUND: usize = 8;
/// An older server's questions in one call, one after another or not.
pub const MAX_QUESTIONS_PER_CALL: usize = DEFAULT_MRTR_MAX_ROUNDS;
const MAX_MESSAGE_BYTES: usize = 4 * 1024;
const MAX_SCHEMA_BYTES: usize = 16 * 1024;
const MAX_SCHEMA_PROPERTIES: usize = 32;

/// Who answers the questions one tool call's server asks. `limited` is true
/// when `message` was cut to fit.
#[async_trait]
pub trait McpElicitor: Send + Sync {
    async fn elicit(
        &self,
        server: &str,
        message: String,
        mode: McpElicitMode,
        limited: bool,
    ) -> McpElicitationAnswer;
}

/// What one `tools/call` carries down to the connection. With no elicitor,
/// every question is answered `cancel`.
#[derive(Clone, Default)]
pub struct McpCallScope {
    pub elicitor: Option<Arc<dyn McpElicitor>>,
    pub tasks: Option<crate::tasks::McpTaskHost>,
}

type Asker = Option<Arc<dyn McpElicitor>>;

/// The requests in flight on one connection that a server's own
/// `elicitation/create` could belong to, each with the token its end cancels.
#[derive(Default)]
pub struct ElicitRoute {
    inflight: Mutex<Vec<Entry>>,
    seq: AtomicU64,
}

struct Entry {
    id: u64,
    elicitor: Asker,
    ended: CancellationToken,
    open: Arc<AtomicUsize>,
    asked: Arc<AtomicUsize>,
}

struct OpenQuestion(Arc<AtomicUsize>);

impl OpenQuestion {
    fn take(open: &Arc<AtomicUsize>) -> Option<Self> {
        let before = open.fetch_add(1, Ordering::SeqCst);
        let held = Self(open.clone());
        (before < MAX_OPEN_PER_CALL).then_some(held)
    }
}

impl Drop for OpenQuestion {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// One request entered in an [`ElicitRoute`], until dropped.
pub struct InFlight {
    route: Arc<ElicitRoute>,
    id: u64,
    asked: Arc<AtomicUsize>,
}

impl InFlight {
    /// The server's own questions in this call that were cancelled for
    /// going over [`MAX_QUESTIONS_PER_CALL`].
    pub fn cut(&self) -> usize {
        self.asked
            .load(Ordering::SeqCst)
            .saturating_sub(MAX_QUESTIONS_PER_CALL)
    }
}

impl ElicitRoute {
    pub fn enter(self: &Arc<Self>, elicitor: Asker) -> InFlight {
        let id = self.seq.fetch_add(1, Ordering::Relaxed);
        let asked = Arc::<AtomicUsize>::default();
        self.lock().push(Entry {
            id,
            elicitor,
            ended: CancellationToken::new(),
            open: Arc::default(),
            asked: asked.clone(),
        });
        InFlight {
            route: self.clone(),
            id,
            asked,
        }
    }

    /// The elicitor of the one request in flight and the token its end
    /// cancels, or how many there are when that is not one.
    fn sole(&self) -> Result<(Asker, CancellationToken, Arc<AtomicUsize>, usize), usize> {
        match self.lock().as_slice() {
            [only] => Ok((
                only.elicitor.clone(),
                only.ended.clone(),
                only.open.clone(),
                only.asked.fetch_add(1, Ordering::SeqCst),
            )),
            all => Err(all.len()),
        }
    }

    /// Answer a server's own `elicitation/create`. The question is withdrawn
    /// when the call it arrived in ends: a server that asks and then returns
    /// does not leave it standing for whatever the host does next.
    pub(crate) async fn answer(&self, server: &str, params: ElicitRequestParams) -> ElicitResult {
        let answer = match self.sole() {
            Ok((_, _, _, before)) if before >= MAX_QUESTIONS_PER_CALL => {
                tracing::warn!(
                    server,
                    "more than {MAX_QUESTIONS_PER_CALL} elicitation requests in one tool call; cancelled"
                );
                McpElicitationAnswer::Cancel
            }
            Ok((elicitor, ended, open, _)) => match OpenQuestion::take(&open) {
                Some(_held) => tokio::select! {
                    biased;
                    () = ended.cancelled() => McpElicitationAnswer::Cancel,
                    answer = elicit(server, params, elicitor.as_deref()) => answer,
                },
                None => {
                    tracing::warn!(
                        server,
                        "more than {MAX_OPEN_PER_CALL} elicitation requests open in one tool call; cancelled"
                    );
                    McpElicitationAnswer::Cancel
                }
            },
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

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Entry>> {
        self.inflight.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        self.route.lock().retain(|entry| {
            let mine = entry.id == self.id;
            if mine {
                entry.ended.cancel();
            }
            !mine
        });
    }
}

/// How `tools/call` ended: a result, or a task the server runs the call as.
pub(crate) enum CallEnd {
    Complete(CallToolResult),
    Task(rmcp::model::Task),
}

/// `tools/call` until the server completes it or turns it into a task,
/// answering each `input_required` round through `scope`. `send` makes one
/// request.
pub(crate) async fn call_with_rounds<F, Fut>(
    server: &str,
    mut params: CallToolRequestParams,
    scope: &McpCallScope,
    mut send: F,
) -> Result<CallEnd, McpError>
where
    F: FnMut(CallToolRequestParams) -> Fut,
    Fut: Future<Output = Result<CallToolResponse, McpError>>,
{
    let mut state_only_rounds = 0u32;
    for _ in 0..DEFAULT_MRTR_MAX_ROUNDS {
        match send(params.clone()).await? {
            CallToolResponse::Complete(result) => return Ok(CallEnd::Complete(result)),
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
            CallToolResponse::Task(created) => return Ok(CallEnd::Task(created.task)),
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
pub(crate) async fn answer_input_requests(
    server: &str,
    requests: InputRequests,
    scope: &McpCallScope,
) -> Result<InputResponses, McpError> {
    let mut responses = InputResponses::new();
    for (asked, (key, request)) in requests.into_iter().enumerate() {
        if asked >= MAX_QUESTIONS_PER_ROUND {
            tracing::warn!(
                server,
                "more than {MAX_QUESTIONS_PER_ROUND} questions in one round; the rest cancelled"
            );
            responses.insert(
                key,
                serde_json::to_value(elicit_result(McpElicitationAnswer::Cancel))?,
            );
            continue;
        }
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
            Ok(requested_schema) if fits_a_form(&requested_schema) => {
                (message, McpElicitMode::Form { requested_schema })
            }
            Ok(_) => {
                tracing::warn!(
                    server,
                    "an elicitation form over the size limits was declined"
                );
                return McpElicitationAnswer::Decline;
            }
            Err(_) => return McpElicitationAnswer::Decline,
        },
        ElicitRequestParams::UrlElicitationParams { message, url, .. } => match openable(&url) {
            Some((url, host)) => (message, McpElicitMode::Url { url, host }),
            None => {
                tracing::warn!(server, "an elicitation URL that is not https was declined");
                return McpElicitationAnswer::Decline;
            }
        },
        _ => return McpElicitationAnswer::Decline,
    };
    let (message, limited) = cut_to_fit(message);
    elicitor.elicit(server, message, mode, limited).await
}

fn fits_a_form(schema: &serde_json::Value) -> bool {
    let properties = schema
        .get("properties")
        .and_then(serde_json::Value::as_object)
        .map_or(0, |p| p.len());
    properties <= MAX_SCHEMA_PROPERTIES
        && serde_json::to_vec(schema).is_ok_and(|bytes| bytes.len() <= MAX_SCHEMA_BYTES)
}

fn cut_to_fit(mut message: String) -> (String, bool) {
    if message.len() <= MAX_MESSAGE_BYTES {
        return (message, false);
    }
    let mut end = MAX_MESSAGE_BYTES;
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    message.truncate(end);
    (message, true)
}

/// `url` as it parses — what a browser would open — and its host, when a
/// person may be sent there: `https`, or `http` to this machine.
fn openable(url: &str) -> Option<(String, String)> {
    let parsed = url::Url::parse(url).ok()?;
    let host = parsed.host_str()?.to_string();
    let loopback = matches!(parsed.host(), Some(url::Host::Domain("localhost")))
        || match parsed.host() {
            Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
            Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
            _ => false,
        };
    match parsed.scheme() {
        "https" => Some((parsed.as_str().to_string(), host)),
        "http" if loopback => Some((parsed.as_str().to_string(), host)),
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
        async fn elicit(
            &self,
            _: &str,
            _: String,
            _: McpElicitMode,
            _: bool,
        ) -> McpElicitationAnswer {
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

    /// Answers once it is let go.
    struct Held(Arc<tokio::sync::Notify>);

    #[async_trait]
    impl McpElicitor for Held {
        async fn elicit(
            &self,
            _: &str,
            _: String,
            _: McpElicitMode,
            _: bool,
        ) -> McpElicitationAnswer {
            self.0.notified().await;
            McpElicitationAnswer::Accept {
                content: Some(serde_json::json!({"color": "blue"})),
            }
        }
    }

    /// A 2025 server's question is the question of the call it arrived in:
    /// once that call is over, an answer given later is not sent.
    #[tokio::test]
    async fn a_question_outliving_its_call_is_cancelled() {
        let route = Arc::new(ElicitRoute::default());
        let release = Arc::new(tokio::sync::Notify::new());
        let call = route.enter(Some(Arc::new(Held(release.clone()))));
        let asking = tokio::spawn({
            let route = route.clone();
            async move { route.answer("demo", form()).await }
        });
        tokio::task::yield_now().await;
        drop(call);
        release.notify_waiters();
        let answered = tokio::time::timeout(std::time::Duration::from_secs(5), asking)
            .await
            .expect("the question ends with its call")
            .unwrap();
        assert_eq!(answered.action, ElicitationAction::Cancel);
    }

    /// Remembers the link it was shown.
    #[derive(Default)]
    struct Shown(Mutex<Option<String>>);

    #[async_trait]
    impl McpElicitor for Shown {
        async fn elicit(
            &self,
            _: &str,
            _: String,
            mode: McpElicitMode,
            _: bool,
        ) -> McpElicitationAnswer {
            if let McpElicitMode::Url { url, .. } = mode {
                *self.0.lock().unwrap() = Some(url);
            }
            McpElicitationAnswer::Cancel
        }
    }

    /// The host is shown the link it would open, not the server's spelling
    /// of it: line breaks and bidi controls cannot make one link look like
    /// another.
    #[tokio::test]
    async fn a_url_is_put_to_the_host_as_it_parses() {
        for link in [
            "https://evil.example/x\n\n  https://accounts.google.com/",
            "https://evil.example/\u{202e}moc.elgoog//:sptth",
        ] {
            let shown = Shown::default();
            elicit("demo", url(link), Some(&shown)).await;
            let got = shown.0.lock().unwrap().clone().unwrap();
            assert!(
                !got.chars().any(|c| c.is_control() || c == '\u{202e}'),
                "{got:?}"
            );
            assert_eq!(got, url::Url::parse(link).unwrap().as_str());
        }
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
    async fn rounds_are_driven_until_the_call_completes_and_a_task_is_handed_back() {
        use rmcp::model::{InputRequiredResult, Task, TaskStatus};
        let scope = McpCallScope {
            elicitor: Some(accepting()),
            tasks: None,
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
        assert!(
            matches!(result, Ok(CallEnd::Complete(_))),
            "{:?}",
            result.err()
        );
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
            matches!(&task, Ok(CallEnd::Task(t)) if t.task_id == "t1"),
            "{:?}",
            task.err()
        );
    }
}
