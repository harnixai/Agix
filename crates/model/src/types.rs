//! Anthropic Messages API 请求体类型。
//!
//! 字段名 / 形状对应 API；与 agix-base::Message / ContentBlock 配合使用。

use base::interface::model::Effort;
use base::message::{ContentBlock, Role};
use serde::Serialize;
use serde_json::Value;

/// Anthropic's hard cap on `cache_control` breakpoints in a single request,
/// counted across `tools` + `system` + `messages` combined. A request with a
/// fifth breakpoint is rejected with a 400, so the automatic breakpoints this
/// module adds and the ones the caller placed share one budget, and the
/// caller's are thinned to fit (see `MessagesRequest::cache_plan`).
pub const MAX_CACHE_BREAKPOINTS: usize = 4;

/// Beta flag historically required to use `ttl: "1h"` on a `cache_control`
/// block. Anthropic has since made the 1-hour TTL generally available and no
/// longer documents the header as a requirement, but it is still accepted, and
/// Anthropic-compatible relays that mirror an older snapshot of the API may
/// still gate on it — sending it costs one header and removes a silent
/// "1h TTL quietly downgraded to 5m" failure mode. See
/// [`MessagesRequest::ensure_cache_ttl_beta`].
pub const EXTENDED_CACHE_TTL_BETA: &str = "extended-cache-ttl-2025-04-11";

/// Built-in Anthropic tool types (e.g., `web_search_20250305`) that go into the
/// `tools` array alongside regular function `ToolDef` entries.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
pub enum BuiltinTool {
    /// Server-side web search. Generally available; no beta header.
    #[serde(rename = "web_search_20250305")]
    WebSearch {
        name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        allowed_domains: Option<Vec<String>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        blocked_domains: Option<Vec<String>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        max_uses: Option<u32>,
    },
}

/// Messages.create 的请求体。`stream: true` 永远开启（loop 是流式的）。
///
/// 注：自定义 `Serialize` —— tools + anthropic_tools 合并成一个 JSON
/// `tools` 数组发送，built-in tools 与 function tools 在同一数组里。
#[derive(Debug, Clone)]
pub struct MessagesRequest {
    pub model: String,
    pub max_tokens: u32,

    /// system prompt 是字符串数组（每段可独立带 cache_control）。
    pub system: Vec<SystemBlock>,

    pub messages: Vec<MessageParam>,

    pub tools: Vec<ToolDef>,

    /// Built-in Anthropic tools (web_search_20250305, etc.) merged into the
    /// same `tools` JSON array as `tools` during serialization.
    pub anthropic_tools: Vec<BuiltinTool>,

    pub tool_choice: Option<ToolChoice>,

    /// 永远 true（流式）
    pub stream: bool,

    /// `None` = 不设字段（让模型按默认走，思考模型仍会思考）。
    /// `Some(Disabled)` = 显式关闭思考（DeepSeek V4 + 部分 Anthropic-compat
    /// 端点接受；-历史 baseline 都用了这条以避免多 turn 400）。
    /// `Some(Enabled{...}) / Some(Adaptive)` = 开思考。
    pub thinking: Option<ThinkingConfig>,

    /// 写成 `output_config.effort`；`None` 不发这个字段。
    pub effort: Option<Effort>,

    /// 当 thinking 开启时**必须** None；否则 API 报错
    pub temperature: Option<f32>,

    pub top_p: Option<f32>,

    pub top_k: Option<u32>,

    pub stop_sequences: Vec<String>,

    /// `{ user_id }`，匿名化后的 device + session 标识；用于 abuse 监控
    pub metadata: Option<RequestMetadata>,

    /// beta header 列表：走 HTTP `anthropic-beta` 头而非 body。
    pub betas: Vec<String>,

    /// fast / extended-output / 任务预算等高级参数
    pub speed: Option<Speed>,
}

impl MessagesRequest {
    /// 一个最小可用的请求构造器：模型 + 一条 user 文字。
    /// system / tools 为空；适合冒烟测试和单元测。
    pub fn minimal(model: impl Into<String>, user_text: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            max_tokens: 1024,
            system: Vec::new(),
            messages: vec![MessageParam {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: user_text.into(),
                    cache_control: None,
                }],
            }],
            tools: Vec::new(),
            anthropic_tools: Vec::new(),
            tool_choice: None,
            stream: true,
            thinking: None,
            effort: None,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: Vec::new(),
            metadata: None,
            betas: Vec::new(),
            speed: None,
        }
    }

    // **Why this lives on the request (and is applied during `Serialize`)
    // rather than in `adapter.rs`.**
    //
    // The API renders a request as `tools` → `system` → `messages`, and the
    // 4-breakpoint budget is shared across all three. Deciding where the
    // automatic breakpoints go therefore needs to see the whole request at
    // once; splitting the decision between "who builds the tools array" and
    // "who builds the messages array" is how you end up emitting five
    // breakpoints and getting a 400 on a long session.
    //
    // The messages breakpoint additionally *cannot* be expressed in the Rust
    // value: `base::message::ContentBlock` only carries a `cache_control`
    // field on its `Text` variant, and in an agentic session the tail of the
    // conversation is overwhelmingly `tool_result` blocks. Rather than mark
    // some arbitrary earlier text block (or, worse, append a synthetic text
    // block — which would change the *content* bytes of an older message and
    // invalidate the very prefix we are trying to cache), the marker is
    // injected into the already-serialized JSON, where any block type can
    // carry it. `cache_control` is metadata, not content: moving a breakpoint
    // between turns creates a new read point without invalidating entries
    // written at earlier positions, which is exactly what makes the rolling
    // scheme below work.

    /// Whether we should append an automatic breakpoint to the **last** entry
    /// of the rendered `tools` array.
    ///
    /// The tool array is the very first thing the API renders and is stable
    /// for the whole session (this repo ships 30+ tools, whose schemas are a
    /// non-trivial share of every request), so a breakpoint on its last entry
    /// caches the entire tool-definition prefix. It is worth having even
    /// though a `system` breakpoint already covers the same bytes: the system
    /// prompt carries runtime state (plan, todos, turn info) and changes
    /// between turns, and when it does, the tools-only entry is the one that
    /// still hits.
    ///
    /// Skipped when the caller placed its own tool breakpoints — an explicit
    /// choice about placement beats a heuristic.
    fn wants_auto_tools_breakpoint(&self) -> bool {
        !(self.tools.is_empty() && self.anthropic_tools.is_empty())
            && self.tools.iter().all(|t| t.cache_control.is_none())
    }

    /// Index into `self.messages` of the message that should carry the rolling
    /// conversation breakpoint, if any.
    ///
    /// **The rule: the second-to-last `user` message.** Not the last one —
    /// that message is new on this turn, so a breakpoint there is a pure cache
    /// *write* that nothing ever reads back. The second-to-last user message
    /// was the last one on the *previous* turn, so the entry written there
    /// last turn is read this turn, and a fresh (longer) entry is written one
    /// message further along. That is the standard rolling scheme: each turn
    /// reads the previous turn's entry and extends it.
    ///
    /// Caveat worth knowing: a breakpoint only searches back ~20 content
    /// blocks for a prior entry, so a single turn that emits more than that
    /// many blocks (a very wide parallel tool fan-out) can miss and pay a full
    /// write. That is still strictly better than today's behaviour, which
    /// re-bills the whole history every turn.
    fn auto_message_breakpoint_index(&self) -> Option<usize> {
        let mut user_indices = self
            .messages
            .iter()
            .enumerate()
            .filter(|(_, m)| m.role == Role::User)
            .map(|(i, _)| i);
        // `nth_back` would need a DoubleEndedIterator; collecting the last two
        // by hand keeps this allocation-free.
        let (mut prev, mut last) = (None, None);
        for i in user_indices.by_ref() {
            prev = last;
            last = Some(i);
        }
        // Needs at least two user turns: with only one, the "second-to-last"
        // doesn't exist and the single turn is the volatile one.
        prev.filter(|&i| self.messages.get(i).is_some_and(|m| !m.content.is_empty()))
    }

    /// The last block of each stable run of cached `system` blocks, in order.
    ///
    /// A run is a stretch of consecutive cached blocks; an uncached block ends
    /// it, and a volatile block (see [`SystemBlock::volatile`]) stands alone,
    /// so the run ahead of it keeps a breakpoint of its own. The first entry
    /// ends the leading run — the static prefix, when the prompt opens with
    /// one — and the last is the last cached block.
    fn system_run_ends(&self) -> Vec<usize> {
        let extends_run = |b: Option<&SystemBlock>| {
            matches!(
                b,
                Some(SystemBlock::Text {
                    cache_control: Some(_),
                    volatile: false,
                    ..
                })
            )
        };
        self.system
            .iter()
            .enumerate()
            .filter(|&(i, b)| match b {
                SystemBlock::Text {
                    cache_control: None,
                    ..
                } => false,
                SystemBlock::Text { volatile: true, .. } => true,
                SystemBlock::Text { .. } => !extends_run(self.system.get(i + 1)),
            })
            .map(|(i, _)| i)
            .collect()
    }

    /// Every `cache_control` marker the serialized request carries, in render
    /// order (`tools` → `system` → `messages`), never more than
    /// [`MAX_CACHE_BREAKPOINTS`].
    ///
    /// Candidates are taken in this order until the budget is spent:
    ///
    /// 1. the caller's last tool breakpoint; the end of the static `system`
    ///    prefix and the last cached `system` block (see
    ///    [`Self::system_run_ends`]); the caller's last message breakpoint;
    /// 2. the automatic tools and rolling-message breakpoints;
    /// 3. the caller's remaining breakpoints, later ones first — `system` run
    ///    ends, then other `system` blocks, then messages, then tools.
    ///
    /// Within a section a later breakpoint caches a longer prefix, so earlier
    /// ones only pay off when something between them changes. The one kept on
    /// purpose is the static-prefix end: it is what still hits after the
    /// dynamic tail of the system prompt changes. The automatic pair outranks
    /// the rest of the caller's `system` markers because prompt assembly marks
    /// every cached block, not a chosen few.
    ///
    /// The API also refuses a 1h entry rendered after a 5m one. Dropping
    /// markers cannot create that order and the automatic tools marker (1h)
    /// renders first; the rolling marker (5m) is left out when a kept 1h
    /// marker follows it.
    fn cache_plan(&self) -> Vec<Breakpoint> {
        let tools = self.caller_tool_marks();
        let messages = self.caller_message_marks();
        let system_cached: Vec<usize> = self
            .system
            .iter()
            .enumerate()
            .filter(|(_, b)| match b {
                SystemBlock::Text { cache_control, .. } => cache_control.is_some(),
            })
            .map(|(i, _)| i)
            .collect();
        let system = |i: usize| self.caller_system_mark(i);
        let run_ends = self.system_run_ends();
        let (auto_tools, auto_message) = self.automatic_marks(&messages);

        let mut candidates: Vec<Breakpoint> = Vec::new();
        candidates.extend(tools.last().copied());
        candidates.extend(run_ends.first().map(|&i| system(i)));
        candidates.extend(run_ends.last().map(|&i| system(i)));
        candidates.extend(messages.last().copied());
        candidates.extend(auto_tools);
        candidates.extend(auto_message);
        candidates.extend(run_ends.iter().rev().map(|&i| system(i)));
        candidates.extend(system_cached.iter().rev().map(|&i| system(i)));
        candidates.extend(messages.iter().rev().copied());
        candidates.extend(tools.iter().rev().copied());
        within_budget(candidates, auto_message)
    }

    fn caller_tool_marks(&self) -> Vec<Breakpoint> {
        self.tools
            .iter()
            .enumerate()
            .filter_map(|(i, t)| {
                let cc = t.cache_control.as_ref()?;
                Some(Breakpoint::explicit(Spot::Tool(i), cc.is_one_hour()))
            })
            .collect()
    }

    fn caller_system_mark(&self, i: usize) -> Breakpoint {
        match &self.system[i] {
            SystemBlock::Text { cache_control, .. } => Breakpoint::explicit(
                Spot::System(i),
                cache_control
                    .as_ref()
                    .is_some_and(CacheControl::is_one_hour),
            ),
        }
    }

    fn caller_message_marks(&self) -> Vec<Breakpoint> {
        self.messages
            .iter()
            .enumerate()
            .flat_map(|(i, m)| {
                m.content
                    .iter()
                    .enumerate()
                    .filter_map(move |(j, b)| match b {
                        ContentBlock::Text {
                            cache_control: Some(cc),
                            ..
                        } => Some(Breakpoint::explicit(
                            Spot::Message(i, j),
                            cc.get("ttl").and_then(Value::as_str) == Some("1h"),
                        )),
                        _ => None,
                    })
            })
            .collect()
    }

    /// The automatic tools-prefix and rolling-message breakpoints, when wanted.
    ///
    /// 1-hour TTL for the tool prefix: the tool array is fixed for the
    /// lifetime of a session, so it should outlive the gaps between turns.
    /// 5 minutes for the rolling marker: it is superseded by the next
    /// turn's, so it only has to survive one round trip, and a 5m write
    /// costs 1.25x base against 2x for 1h.
    fn automatic_marks(
        &self,
        caller_messages: &[Breakpoint],
    ) -> (Option<Breakpoint>, Option<Breakpoint>) {
        let auto_tools = self.wants_auto_tools_breakpoint().then(|| Breakpoint {
            spot: Spot::Tool(self.tools.len() + self.anthropic_tools.len() - 1),
            one_hour: true,
            automatic: true,
        });
        let auto_message = self
            .auto_message_breakpoint_index()
            .map(|i| Spot::Message(i, self.messages[i].content.len() - 1))
            .filter(|spot| !caller_messages.iter().any(|b| b.spot == *spot))
            .map(|spot| Breakpoint {
                spot,
                one_hour: false,
                automatic: true,
            });
        (auto_tools, auto_message)
    }

    /// True when any `cache_control` this request will emit uses the 1-hour
    /// TTL — including the automatic tools breakpoint, which does.
    pub fn uses_1h_cache_ttl(&self) -> bool {
        self.cache_plan().iter().any(|b| b.one_hour)
    }

    /// Add [`EXTENDED_CACHE_TTL_BETA`] to `betas` when this request uses a
    /// 1-hour cache TTL anywhere. Idempotent.
    ///
    /// Called by the adapter after the request is fully built, so the flag
    /// tracks what will actually be serialized rather than what the caller
    /// intended.
    pub fn ensure_cache_ttl_beta(&mut self) {
        if self.uses_1h_cache_ttl() && !self.betas.iter().any(|b| b == EXTENDED_CACHE_TTL_BETA) {
            self.betas.push(EXTENDED_CACHE_TTL_BETA.to_string());
        }
    }
}

/// A place a `cache_control` marker can sit. The derived order is the API's
/// render order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Spot {
    Tool(usize),
    System(usize),
    Message(usize, usize),
}

#[derive(Debug, Clone, Copy)]
struct Breakpoint {
    spot: Spot,
    one_hour: bool,
    automatic: bool,
}

impl Breakpoint {
    fn explicit(spot: Spot, one_hour: bool) -> Self {
        Self {
            spot,
            one_hour,
            automatic: false,
        }
    }
}

/// Make the serialized `tools`, `system` and `messages` carry exactly the
/// markers in `plan`: the caller's markers left out of it are removed and the
/// automatic ones written in.
fn apply_cache_plan(
    plan: &[Breakpoint],
    tools: &mut [Value],
    system: &mut [Value],
    messages: &mut [Value],
) {
    let strip = |v: &mut Value, spot: Spot| {
        if plan.iter().all(|b| b.spot != spot) {
            if let Some(obj) = v.as_object_mut() {
                obj.remove("cache_control");
            }
        }
    };
    for (i, t) in tools.iter_mut().enumerate() {
        strip(t, Spot::Tool(i));
    }
    for (i, b) in system.iter_mut().enumerate() {
        strip(b, Spot::System(i));
    }
    for (i, m) in messages.iter_mut().enumerate() {
        if let Some(blocks) = m.get_mut("content").and_then(Value::as_array_mut) {
            for (j, b) in blocks.iter_mut().enumerate() {
                strip(b, Spot::Message(i, j));
            }
        }
    }
    for bp in plan.iter().filter(|b| b.automatic) {
        let target = match bp.spot {
            Spot::Tool(i) => tools.get_mut(i),
            Spot::System(i) => system.get_mut(i),
            Spot::Message(i, j) => messages
                .get_mut(i)
                .and_then(|m| m.get_mut("content"))
                .and_then(|c| c.get_mut(j)),
        };
        let cc = match bp.one_hour {
            true => CacheControl::ephemeral_1h(),
            false => CacheControl::ephemeral_5m(),
        };
        if let Some(target) = target {
            set_cache_control(target, &cc);
        }
    }
}

/// The first [`MAX_CACHE_BREAKPOINTS`] distinct `candidates`, in render
/// order, without the rolling marker when a kept 1h marker renders after it.
fn within_budget(candidates: Vec<Breakpoint>, rolling: Option<Breakpoint>) -> Vec<Breakpoint> {
    let mut kept: Vec<Breakpoint> = Vec::with_capacity(MAX_CACHE_BREAKPOINTS);
    for bp in candidates {
        if kept.len() == MAX_CACHE_BREAKPOINTS {
            break;
        }
        if !kept.iter().any(|k| k.spot == bp.spot) {
            kept.push(bp);
        }
    }
    if let Some(rolling) = rolling {
        if kept.iter().any(|b| b.one_hour && b.spot > rolling.spot) {
            kept.retain(|b| b.spot != rolling.spot);
        }
    }
    kept.sort_by_key(|b| b.spot);
    kept
}

/// Write `cache_control` into an already-serialized content-block / tool
/// object. No-op if the value isn't a JSON object (which would mean the wire
/// format changed out from under us) or already carries the key.
fn set_cache_control(target: &mut Value, cc: &CacheControl) {
    let Some(obj) = target.as_object_mut() else {
        return;
    };
    if obj.contains_key("cache_control") {
        return;
    }
    match serde_json::to_value(cc) {
        Ok(v) => {
            obj.insert("cache_control".to_string(), v);
        }
        // `CacheControl` is a plain enum of string fields; this cannot fail in
        // practice, and a missing breakpoint is a cost regression, not a
        // correctness one — so it degrades rather than aborting the request.
        Err(e) => tracing::warn!(error = %e, "failed to serialize cache_control"),
    }
}

// Custom Serialize: merges `tools` and `anthropic_tools` into a single
// `tools` JSON array so built-in tools (web_search_20250305) appear in the
// same array as function tools, matching the Anthropic Messages API contract.
//
// It is also where the prompt-cache breakpoints are settled — see the block
// comment on `MessagesRequest`'s breakpoint helpers for why the decision has
// to happen here, with the whole request in view.
impl Serialize for MessagesRequest {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;

        fn values<T: Serialize, E: serde::ser::Error>(
            items: impl IntoIterator<Item = T>,
        ) -> Result<Vec<Value>, E> {
            items
                .into_iter()
                .map(|t| serde_json::to_value(t).map_err(E::custom))
                .collect()
        }

        let mut system: Vec<Value> = values(&self.system)?;
        let mut all_tools: Vec<Value> = values(&self.tools)?;
        all_tools.extend(values::<_, S::Error>(&self.anthropic_tools)?);
        let mut messages: Vec<Value> = values(&self.messages)?;
        apply_cache_plan(
            &self.cache_plan(),
            &mut all_tools,
            &mut system,
            &mut messages,
        );

        let mut map = s.serialize_map(None)?;
        map.serialize_entry("model", &self.model)?;
        map.serialize_entry("max_tokens", &self.max_tokens)?;
        map.serialize_entry("system", &system)?;
        map.serialize_entry("messages", &messages)?;
        map.serialize_entry("stream", &self.stream)?;
        map.serialize_entry("tools", &all_tools)?;

        // Optional fields (only if non-None / non-empty)
        if let Some(ref tc) = self.tool_choice {
            map.serialize_entry("tool_choice", tc)?;
        }
        if let Some(ref th) = self.thinking {
            map.serialize_entry("thinking", th)?;
        }
        if let Some(effort) = self.effort {
            map.serialize_entry("output_config", &serde_json::json!({ "effort": effort }))?;
        }
        if let Some(ref t) = self.temperature {
            map.serialize_entry("temperature", t)?;
        }
        if let Some(ref p) = self.top_p {
            map.serialize_entry("top_p", p)?;
        }
        if let Some(ref k) = self.top_k {
            map.serialize_entry("top_k", k)?;
        }
        if !self.stop_sequences.is_empty() {
            map.serialize_entry("stop_sequences", &self.stop_sequences)?;
        }
        if let Some(ref m) = self.metadata {
            map.serialize_entry("metadata", m)?;
        }
        if let Some(ref sp) = self.speed {
            map.serialize_entry("speed", sp)?;
        }

        map.end()
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SystemBlock {
    Text {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
        #[serde(skip)]
        volatile: bool,
    },
}

impl SystemBlock {
    pub fn text(s: impl Into<String>) -> Self {
        Self::Text {
            text: s.into(),
            cache_control: None,
            volatile: false,
        }
    }
    pub fn text_cached(s: impl Into<String>, cc: CacheControl) -> Self {
        Self::Text {
            text: s.into(),
            cache_control: Some(cc),
            volatile: false,
        }
    }

    /// Marks a block whose text may change between calls, so it never extends
    /// the stable run of cached blocks ahead of it and that run's breakpoint
    /// survives the trimming to [`MAX_CACHE_BREAKPOINTS`]. Not sent.
    pub fn volatile(self) -> Self {
        match self {
            Self::Text {
                text,
                cache_control,
                ..
            } => Self::Text {
                text,
                cache_control,
                volatile: true,
            },
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct MessageParam {
    pub role: Role,
    pub content: Vec<ContentBlock>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,
    /// ToolSearch 延迟加载标志
    #[serde(skip_serializing_if = "Option::is_none")]
    pub defer_loading: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub strict: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolChoice {
    Auto {
        #[serde(skip_serializing_if = "Option::is_none")]
        disable_parallel_tool_use: Option<bool>,
    },
    Any {
        #[serde(skip_serializing_if = "Option::is_none")]
        disable_parallel_tool_use: Option<bool>,
    },
    Tool {
        name: String,
    },
    None,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CacheControl {
    Ephemeral {
        #[serde(skip_serializing_if = "Option::is_none")]
        ttl: Option<CacheTtl>,
    },
}

impl CacheControl {
    pub fn ephemeral_5m() -> Self {
        Self::Ephemeral {
            ttl: Some(CacheTtl::FiveMin),
        }
    }
    pub fn ephemeral_1h() -> Self {
        Self::Ephemeral {
            ttl: Some(CacheTtl::OneHour),
        }
    }

    /// Whether this breakpoint asks for the 1-hour TTL, which is what gates
    /// [`EXTENDED_CACHE_TTL_BETA`].
    pub fn is_one_hour(&self) -> bool {
        matches!(
            self,
            Self::Ephemeral {
                ttl: Some(CacheTtl::OneHour),
                ..
            }
        )
    }
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub enum CacheTtl {
    #[serde(rename = "5m")]
    FiveMin,
    #[serde(rename = "1h")]
    OneHour,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ThinkingConfig {
    /// 模型自适应预算（部分新模型支持；不支持的模型会报错）
    Adaptive,
    /// 显式 token 预算
    Enabled { budget_tokens: u32 },
    /// Explicit "no thinking". DeepSeek V4 (thinking model
    /// by default) and some Anthropic-compat backends accept this to
    /// suppress reasoning_content emission. Important for multi-turn
    /// flows where the client doesn't echo thinking blocks back — DS V4
    /// rejects with 400 in that case.
    Disabled,
    /// Thinking off on a model that refuses `disabled` (Sonnet 5.5). Takes
    /// no other field.
    BetweenTools,
}

#[derive(Debug, Clone, Serialize)]
pub struct RequestMetadata {
    pub user_id: String,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Speed {
    Fast,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn minimal_request_serializes() {
        let req = MessagesRequest::minimal("claude-sonnet-4-6", "hello");
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["model"], "claude-sonnet-4-6");
        assert_eq!(v["max_tokens"], 1024);
        assert_eq!(v["stream"], true);
        assert_eq!(v["system"], json!([]));
        assert_eq!(v["messages"][0]["role"], "user");
        assert_eq!(v["messages"][0]["content"][0]["type"], "text");
        // betas 不进 body
        assert!(v.get("betas").is_none());
        // 可选字段缺省全省略
        assert!(v.get("temperature").is_none());
        assert!(v.get("thinking").is_none());
        assert!(v.get("tool_choice").is_none());
    }

    #[test]
    fn cache_control_serializes_5m_and_1h() {
        let cc5 = CacheControl::ephemeral_5m();
        let cc1 = CacheControl::ephemeral_1h();
        assert_eq!(
            serde_json::to_value(&cc5).unwrap(),
            json!({"type": "ephemeral", "ttl": "5m"})
        );
        assert_eq!(
            serde_json::to_value(&cc1).unwrap(),
            json!({"type": "ephemeral", "ttl": "1h"})
        );
    }

    #[test]
    fn tool_def_with_cache_control() {
        let t = ToolDef {
            name: "Bash".into(),
            description: "run shell".into(),
            input_schema: json!({"type": "object"}),
            cache_control: Some(CacheControl::ephemeral_1h()),
            defer_loading: None,
            strict: None,
        };
        let v = serde_json::to_value(&t).unwrap();
        assert_eq!(v["name"], "Bash");
        assert_eq!(v["cache_control"]["type"], "ephemeral");
        assert!(v.get("defer_loading").is_none());
    }

    #[test]
    fn thinking_enabled_with_budget() {
        let t = ThinkingConfig::Enabled {
            budget_tokens: 8000,
        };
        assert_eq!(
            serde_json::to_value(&t).unwrap(),
            json!({"type": "enabled", "budget_tokens": 8000})
        );
    }

    #[test]
    fn tool_choice_variants() {
        assert_eq!(
            serde_json::to_value(ToolChoice::Auto {
                disable_parallel_tool_use: None
            })
            .unwrap(),
            json!({"type": "auto"})
        );
        assert_eq!(
            serde_json::to_value(ToolChoice::Tool {
                name: "Read".into()
            })
            .unwrap(),
            json!({"type": "tool", "name": "Read"})
        );
        assert_eq!(
            serde_json::to_value(ToolChoice::None).unwrap(),
            json!({"type": "none"})
        );
    }

    #[test]
    fn empty_stop_sequences_omitted() {
        let req = MessagesRequest::minimal("m", "u");
        let v = serde_json::to_value(&req).unwrap();
        assert!(v.get("stop_sequences").is_none());
    }

    #[test]
    fn thinking_disabled_serializes() {
        let t = ThinkingConfig::Disabled;
        assert_eq!(
            serde_json::to_value(&t).unwrap(),
            json!({"type": "disabled"})
        );
    }

    #[test]
    fn thinking_between_tools_serializes_with_no_other_field() {
        assert_eq!(
            serde_json::to_value(ThinkingConfig::BetweenTools).unwrap(),
            json!({"type": "between_tools"})
        );
    }

    /// Full request snapshot — system blocks with cache breakpoints, tools,
    /// messages, and thinking enabled. Verifies the wire shape matches the
    /// Anthropic Messages API contract.
    #[test]
    fn full_request_snapshot() {
        let req = MessagesRequest {
            model: "claude-sonnet-4-6".into(),
            max_tokens: 4096,
            system: vec![
                SystemBlock::text_cached("Core instructions", CacheControl::ephemeral_5m()),
                SystemBlock::text("Dynamic context"),
            ],
            messages: vec![MessageParam {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: "hello".into(),
                    cache_control: None,
                }],
            }],
            tools: vec![ToolDef {
                name: "Bash".into(),
                description: "Execute shell commands".into(),
                input_schema: json!({"type": "object", "properties": {"cmd": {"type": "string"}}}),
                cache_control: Some(CacheControl::ephemeral_5m()),
                defer_loading: None,
                strict: None,
            }],
            anthropic_tools: vec![BuiltinTool::WebSearch {
                name: "web_search".into(),
                allowed_domains: None,
                blocked_domains: None,
                max_uses: Some(8),
            }],
            tool_choice: Some(ToolChoice::Auto {
                disable_parallel_tool_use: None,
            }),
            stream: true,
            thinking: Some(ThinkingConfig::Enabled {
                budget_tokens: 8000,
            }),
            effort: Some(Effort::High),
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: vec![],
            metadata: None,
            betas: vec![],
            speed: None,
        };

        let v = serde_json::to_value(&req).unwrap();
        let obj = v.as_object().unwrap();

        assert_eq!(obj["model"], "claude-sonnet-4-6");
        assert_eq!(obj["max_tokens"], 4096);
        assert_eq!(obj["stream"], true);

        let sys = obj["system"].as_array().unwrap();
        assert_eq!(sys.len(), 2);
        assert_eq!(sys[0]["type"], "text");
        assert_eq!(sys[0]["text"], "Core instructions");
        assert_eq!(sys[0]["cache_control"]["type"], "ephemeral");
        assert_eq!(sys[0]["cache_control"]["ttl"], "5m");
        assert_eq!(sys[1]["type"], "text");
        assert_eq!(sys[1]["text"], "Dynamic context");
        // Second block has no cache_control
        assert!(sys[1].get("cache_control").is_none());

        let msgs = obj["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[0]["content"][0]["type"], "text");
        assert_eq!(msgs[0]["content"][0]["text"], "hello");

        let tools = obj["tools"].as_array().unwrap();
        assert_eq!(
            tools.len(),
            2,
            "tools array should contain both function and built-in tools"
        );
        // Index 0: function tool
        assert_eq!(tools[0]["name"], "Bash");
        assert!(tools[0].get("input_schema").is_some());
        assert_eq!(tools[0]["cache_control"]["type"], "ephemeral");
        // Index 1: built-in web search
        assert_eq!(tools[1]["type"], "web_search_20250305");
        assert_eq!(tools[1]["name"], "web_search");
        assert_eq!(tools[1]["max_uses"], 8);
        assert!(tools[1].get("input_schema").is_none());

        assert_eq!(obj["thinking"]["type"], "enabled");
        assert_eq!(obj["thinking"]["budget_tokens"], 8000);
        assert_eq!(obj["output_config"], json!({"effort": "high"}));

        assert!(obj.get("temperature").is_none());
        assert!(obj.get("top_p").is_none());
        assert!(obj.get("metadata").is_none());
        // betas never appears in body
        assert!(obj.get("betas").is_none());
        // empty stop_sequences is omitted
        assert!(obj.get("stop_sequences").is_none());

        assert_eq!(obj["tool_choice"]["type"], "auto");
    }

    // These assert on the *emitted JSON*, not on the Rust value, because the
    // messages breakpoint only exists in the serialized form (see the block
    // comment on `MessagesRequest`'s breakpoint helpers).

    fn tool(name: &str) -> ToolDef {
        ToolDef {
            name: name.into(),
            description: "t".into(),
            input_schema: json!({"type": "object"}),
            cache_control: None,
            defer_loading: None,
            strict: None,
        }
    }

    fn text_msg(role: Role, text: &str) -> MessageParam {
        MessageParam {
            role,
            content: vec![ContentBlock::Text {
                text: text.into(),
                cache_control: None,
            }],
        }
    }

    /// Every `cache_control` in the request, as `(location, ttl)` pairs in
    /// render order. Counting these is the whole point: more than four is a
    /// 400. A marker without a TTL is the 5m default.
    fn breakpoints(v: &Value) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for (i, t) in v["tools"].as_array().into_iter().flatten().enumerate() {
            if let Some(cc) = t.get("cache_control") {
                out.push((
                    format!("tools[{i}]"),
                    cc["ttl"].as_str().unwrap_or("5m").into(),
                ));
            }
        }
        for (i, b) in v["system"].as_array().into_iter().flatten().enumerate() {
            if let Some(cc) = b.get("cache_control") {
                out.push((
                    format!("system[{i}]"),
                    cc["ttl"].as_str().unwrap_or("5m").into(),
                ));
            }
        }
        for (i, m) in v["messages"].as_array().into_iter().flatten().enumerate() {
            for (j, b) in m["content"].as_array().into_iter().flatten().enumerate() {
                if let Some(cc) = b.get("cache_control") {
                    out.push((
                        format!("messages[{i}].content[{j}]"),
                        cc["ttl"].as_str().unwrap_or("5m").into(),
                    ));
                }
            }
        }
        out
    }

    fn request_with(
        system: Vec<SystemBlock>,
        tools: Vec<ToolDef>,
        messages: Vec<MessageParam>,
    ) -> MessagesRequest {
        MessagesRequest {
            system,
            tools,
            messages,
            ..MessagesRequest::minimal("m", "ignored")
        }
    }

    /// The last tool gets a breakpoint so the whole
    /// tool-definition prefix is cached, and the *second-to-last* user message
    /// gets the rolling conversation breakpoint. The last user message must
    /// NOT be marked — it is new this turn, so a breakpoint there could only
    /// ever be written, never read.
    #[test]
    fn tools_tail_and_second_to_last_user_message_get_breakpoints() {
        let req = request_with(
            vec![],
            vec![tool("Bash"), tool("Read"), tool("Write")],
            vec![
                text_msg(Role::User, "first"),
                text_msg(Role::Assistant, "ok"),
                text_msg(Role::User, "second"),
                text_msg(Role::Assistant, "ok"),
                text_msg(Role::User, "third — this turn's message"),
            ],
        );
        let v = serde_json::to_value(&req).unwrap();

        assert_eq!(
            breakpoints(&v),
            vec![
                // last tool, 1h — the tool array is fixed for the session
                ("tools[2]".to_string(), "1h".to_string()),
                // messages[2] is the second-to-last *user* message
                ("messages[2].content[0]".to_string(), "5m".to_string()),
            ],
            "got {v:#}"
        );
    }

    /// A tool-use turn's user message is all `tool_result` blocks, which
    /// `base::message::ContentBlock` cannot carry a `cache_control` field on.
    /// The marker still has to land there — that is where the tokens are —
    /// which is why it is injected into the serialized JSON.
    #[test]
    fn breakpoint_lands_on_a_tool_result_block() {
        use base::message::ToolResultContent;
        let tool_results = MessageParam {
            role: Role::User,
            content: vec![
                ContentBlock::ToolResult {
                    tool_use_id: "t1".into(),
                    content: ToolResultContent::text("a"),
                    is_error: false,
                },
                ContentBlock::ToolResult {
                    tool_use_id: "t2".into(),
                    content: ToolResultContent::text("b"),
                    is_error: false,
                },
            ],
        };
        let req = request_with(
            vec![],
            vec![],
            vec![
                text_msg(Role::User, "go"),
                text_msg(Role::Assistant, "calling tools"),
                tool_results,
                text_msg(Role::Assistant, "done"),
                text_msg(Role::User, "thanks"),
            ],
        );
        let v = serde_json::to_value(&req).unwrap();

        // messages[2] is the second-to-last user message; the marker goes on
        // its *last* block so the whole message is inside the cached prefix.
        assert_eq!(
            breakpoints(&v),
            vec![("messages[2].content[1]".to_string(), "5m".to_string())],
            "got {v:#}"
        );
        assert_eq!(v["messages"][2]["content"][1]["type"], "tool_result");
    }

    /// Eight static blocks and a volatile tail, all cached: the code scene's
    /// system prompt.
    fn code_scene_system() -> Vec<SystemBlock> {
        let mut system: Vec<SystemBlock> = (0..8)
            .map(|i| SystemBlock::text_cached(format!("static {i}"), CacheControl::ephemeral_1h()))
            .collect();
        system.push(SystemBlock::text_cached("dynamic", CacheControl::ephemeral_1h()).volatile());
        system
    }

    /// `users` user messages with an assistant reply between each pair.
    fn conversation(users: usize) -> Vec<MessageParam> {
        (0..users * 2 - 1)
            .map(|i| match i % 2 {
                0 => text_msg(Role::User, &format!("u{i}")),
                _ => text_msg(Role::Assistant, &format!("a{i}")),
            })
            .collect()
    }

    fn locations(bps: &[(String, String)]) -> Vec<&str> {
        bps.iter().map(|(l, _)| l.as_str()).collect()
    }

    /// The API refuses a 1h entry rendered after a 5m one.
    fn assert_ttl_order(bps: &[(String, String)]) {
        let first_5m = bps.iter().position(|(_, ttl)| ttl != "1h");
        if let Some(p) = first_5m {
            assert!(
                bps[p..].iter().all(|(_, ttl)| ttl != "1h"),
                "1h after 5m: {bps:?}"
            );
        }
    }

    /// Nine system markers, a tool array and a conversation: what a code
    /// session sends. The tool prefix, the end of the static run, the
    /// volatile tail and the rolling message marker stay.
    #[test]
    fn nine_cached_system_blocks_are_thinned_to_fit() {
        let req = request_with(
            code_scene_system(),
            vec![tool("Bash"), tool("Read")],
            conversation(3),
        );
        let bps = breakpoints(&serde_json::to_value(&req).unwrap());
        assert_eq!(
            bps,
            vec![
                ("tools[1]".to_string(), "1h".to_string()),
                ("system[7]".to_string(), "1h".to_string()),
                ("system[8]".to_string(), "1h".to_string()),
                ("messages[2].content[0]".to_string(), "5m".to_string()),
            ]
        );
        assert_ttl_order(&bps);
    }

    /// With nothing automatic to place, the spare slots go to the later
    /// `system` blocks.
    #[test]
    fn without_tools_or_history_the_spare_slots_go_to_system() {
        let req = request_with(code_scene_system(), vec![], conversation(1));
        let bps = breakpoints(&serde_json::to_value(&req).unwrap());
        assert_eq!(
            locations(&bps),
            ["system[5]", "system[6]", "system[7]", "system[8]"]
        );
    }

    #[test]
    fn no_messages_keeps_tools_and_three_system_markers() {
        let req = request_with(code_scene_system(), vec![tool("Bash")], vec![]);
        let bps = breakpoints(&serde_json::to_value(&req).unwrap());
        assert_eq!(
            locations(&bps),
            ["tools[0]", "system[6]", "system[7]", "system[8]"]
        );
    }

    /// An uncached block ends a run just as a volatile one does.
    #[test]
    fn an_uncached_block_ends_the_static_prefix() {
        let mut system: Vec<SystemBlock> = (0..3)
            .map(|i| SystemBlock::text_cached(format!("s{i}"), CacheControl::ephemeral_1h()))
            .collect();
        system.push(SystemBlock::text("plain"));
        system.extend(
            (0..3).map(|i| SystemBlock::text_cached(format!("t{i}"), CacheControl::ephemeral_1h())),
        );
        let req = request_with(system, vec![tool("Bash")], conversation(2));
        let bps = breakpoints(&serde_json::to_value(&req).unwrap());
        assert_eq!(
            locations(&bps),
            [
                "tools[0]",
                "system[2]",
                "system[6]",
                "messages[0].content[0]"
            ]
        );
    }

    /// The caller's tool markers suppress the automatic one and, past the
    /// budget, only the last of them stays; the rolling marker takes the
    /// slot left after the two key `system` markers.
    #[test]
    fn explicit_tool_breakpoints_are_thinned_too() {
        let tools = (0..6)
            .map(|i| {
                let mut t = tool(&format!("T{i}"));
                t.cache_control = Some(CacheControl::ephemeral_1h());
                t
            })
            .collect();
        let req = request_with(code_scene_system(), tools, conversation(3));
        let bps = breakpoints(&serde_json::to_value(&req).unwrap());
        assert_eq!(
            locations(&bps),
            [
                "tools[5]",
                "system[7]",
                "system[8]",
                "messages[2].content[0]"
            ]
        );
        assert_ttl_order(&bps);
    }

    #[test]
    fn explicit_tool_breakpoints_alone_keep_the_last_four() {
        let tools = (0..6)
            .map(|i| {
                let mut t = tool(&format!("T{i}"));
                t.cache_control = Some(CacheControl::ephemeral_5m());
                t
            })
            .collect();
        let req = request_with(vec![], tools, vec![]);
        let bps = breakpoints(&serde_json::to_value(&req).unwrap());
        assert_eq!(
            locations(&bps),
            ["tools[2]", "tools[3]", "tools[4]", "tools[5]"]
        );
    }

    /// A caller's marker on a message text block counts against the budget
    /// and outranks the automatic ones: here the rolling marker is the one
    /// left out.
    #[test]
    fn explicit_message_breakpoints_count_and_win_over_automatic_ones() {
        let mut messages = conversation(3);
        messages[0].content = vec![ContentBlock::Text {
            text: "pinned".into(),
            cache_control: Some(json!({"type": "ephemeral"})),
        }];
        let req = request_with(code_scene_system(), vec![tool("Bash")], messages);
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(
            locations(&breakpoints(&v)),
            [
                "tools[0]",
                "system[7]",
                "system[8]",
                "messages[0].content[0]"
            ]
        );
        assert_eq!(
            v["messages"][0]["content"][0]["cache_control"],
            json!({"type": "ephemeral"})
        );
    }

    /// The rolling marker is 5m; a 1h marker the caller placed after it in the
    /// conversation would make the request invalid, so the rolling one yields.
    #[test]
    fn rolling_marker_never_precedes_a_one_hour_marker() {
        let mut messages = conversation(3);
        messages[4].content = vec![ContentBlock::Text {
            text: "latest".into(),
            cache_control: Some(json!({"type": "ephemeral", "ttl": "1h"})),
        }];
        let req = request_with(vec![], vec![], messages);
        let bps = breakpoints(&serde_json::to_value(&req).unwrap());
        assert_eq!(
            bps,
            vec![("messages[4].content[0]".to_string(), "1h".to_string())]
        );
    }

    /// The beta flag follows what is sent, not what the caller marked.
    #[test]
    fn a_trimmed_one_hour_marker_does_not_drag_in_the_beta_flag() {
        let tools = (0..5)
            .map(|i| {
                let mut t = tool(&format!("T{i}"));
                t.cache_control = Some(match i {
                    0 => CacheControl::ephemeral_1h(),
                    _ => CacheControl::ephemeral_5m(),
                });
                t
            })
            .collect();
        let req = request_with(vec![], tools, vec![]);
        let bps = breakpoints(&serde_json::to_value(&req).unwrap());
        assert!(!locations(&bps).contains(&"tools[0]"), "{bps:?}");
        assert!(!req.uses_1h_cache_ttl());
    }

    /// An explicit tool breakpoint means the caller has made a placement
    /// decision; don't second-guess it by adding another.
    #[test]
    fn explicit_tool_breakpoint_suppresses_the_automatic_one() {
        let mut first = tool("Bash");
        first.cache_control = Some(CacheControl::ephemeral_5m());
        let req = request_with(vec![], vec![first, tool("Read")], vec![]);
        let v = serde_json::to_value(&req).unwrap();

        assert_eq!(
            breakpoints(&v),
            vec![("tools[0]".to_string(), "5m".to_string())],
            "got {v:#}"
        );
    }

    /// A single-turn request (the very first message of a session, or a
    /// one-shot auxiliary call) has no stable conversation prefix to cache —
    /// marking the only user message would be a write nothing reads.
    #[test]
    fn single_user_turn_gets_no_message_breakpoint() {
        let req = request_with(vec![], vec![], vec![text_msg(Role::User, "hello")]);
        let v = serde_json::to_value(&req).unwrap();
        assert!(breakpoints(&v).is_empty(), "got {v:#}");
    }

    /// Empty tool array ⇒ no tool breakpoint (there is no entry to put it on,
    /// and an empty `tools: []` prefix is worth nothing).
    #[test]
    fn empty_tools_get_no_breakpoint() {
        let req = request_with(vec![], vec![], vec![]);
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["tools"], json!([]));
        assert!(breakpoints(&v).is_empty());
    }

    /// The built-in tool array participates too: `anthropic_tools` are merged
    /// into the same JSON array, so the breakpoint belongs on whatever ends up
    /// last, built-in or not.
    #[test]
    fn breakpoint_goes_on_the_merged_tail_including_builtins() {
        let req = MessagesRequest {
            tools: vec![tool("Bash")],
            anthropic_tools: vec![BuiltinTool::WebSearch {
                name: "web_search".into(),
                allowed_domains: None,
                blocked_domains: None,
                max_uses: None,
            }],
            ..MessagesRequest::minimal("m", "hi")
        };
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(
            breakpoints(&v),
            vec![("tools[1]".to_string(), "1h".to_string())],
            "got {v:#}"
        );
        assert_eq!(v["tools"][1]["type"], "web_search_20250305");
    }

    /// The 1h TTL used by the tool breakpoint drags in the beta flag that
    /// gates it, and doing so twice must not duplicate the header value.
    #[test]
    fn ensure_cache_ttl_beta_is_added_once_when_1h_is_used() {
        let mut req = MessagesRequest {
            tools: vec![tool("Bash")],
            ..MessagesRequest::minimal("m", "hi")
        };
        assert!(req.uses_1h_cache_ttl());
        req.ensure_cache_ttl_beta();
        req.ensure_cache_ttl_beta();
        assert_eq!(req.betas, vec![EXTENDED_CACHE_TTL_BETA.to_string()]);
    }

    /// No 1h TTL anywhere (no tools, no cached system blocks) ⇒ no beta flag.
    /// The 5m messages breakpoint does not need it.
    #[test]
    fn no_beta_flag_without_a_1h_ttl() {
        let mut req = request_with(
            vec![SystemBlock::text("plain")],
            vec![],
            vec![
                text_msg(Role::User, "a"),
                text_msg(Role::Assistant, "b"),
                text_msg(Role::User, "c"),
            ],
        );
        assert!(!req.uses_1h_cache_ttl());
        req.ensure_cache_ttl_beta();
        assert!(req.betas.is_empty());
    }
}
