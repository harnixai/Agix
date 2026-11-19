//! Stage aggregation: the lead's model reduces a stage's member outputs to
//! the best one or a merged document (`concat` leaves them as they are).

use base::interface::model::ThinkingMode;
use base::interface::model::{Model, ModelEvent, StreamParams};
use base::prompt::{BlockRole, PromptBlock};
use futures::stream::StreamExt;
use tokio_util::sync::CancellationToken;

use crate::tool::AggregateMode;

pub async fn aggregate(
    model: &dyn Model,
    model_name: &str,
    mode: AggregateMode,
    stage_name: &str,
    sections: &[(String, String, bool)],
    session_id: Option<&str>,
    cancel: &CancellationToken,
) -> Vec<(String, String, bool)> {
    match mode {
        AggregateMode::Concat => sections.to_vec(),
        AggregateMode::Best => {
            if let Some(label) =
                pick_best(model, model_name, stage_name, sections, session_id, cancel).await
            {
                sections
                    .iter()
                    .filter(|(l, _, _)| *l == label)
                    .cloned()
                    .collect()
            } else {
                sections.to_vec()
            }
        }
        AggregateMode::Aggregate => {
            let text = merge(model, model_name, stage_name, sections, session_id, cancel)
                .await
                .unwrap_or_else(|| {
                    let mut a = String::new();
                    for (l, b, _) in sections {
                        a.push_str(&format!("### {l}\n{b}\n\n"));
                    }
                    a
                });
            vec![("(aggregated)".into(), text, false)]
        }
    }
}

async fn pick_best(
    model: &dyn Model,
    model_name: &str,
    stage_name: &str,
    sections: &[(String, String, bool)],
    session_id: Option<&str>,
    cancel: &CancellationToken,
) -> Option<String> {
    let formatted: Vec<String> = sections
        .iter()
        .map(|(l, b, e)| {
            let s = if *e { " (ERROR)" } else { "" };
            format!("<agent label=\"{l}{s}\">\n{b}\n</agent>")
        })
        .collect();
    let prompt = format!(
        "You are evaluating results of a team of AI agents on stage \"{stage_name}\".\n\n\
         Results:\n{}\n\n\
         Pick the single best result. Return ONLY the agent label, nothing else.",
        formatted.join("\n\n"),
    );
    let label = drain(model, model_name, prompt, 100, session_id, cancel).await?;
    let label = label.trim().to_string();
    if sections.iter().any(|(l, _, _)| l == &label) {
        Some(label)
    } else {
        None
    }
}

async fn merge(
    model: &dyn Model,
    model_name: &str,
    stage_name: &str,
    sections: &[(String, String, bool)],
    session_id: Option<&str>,
    cancel: &CancellationToken,
) -> Option<String> {
    let formatted: Vec<String> = sections
        .iter()
        .map(|(l, b, e)| {
            let s = if *e { " (ERROR)" } else { "" };
            format!("<agent label=\"{l}{s}\">\n{b}\n</agent>")
        })
        .collect();
    let prompt = format!(
        "Synthesize results of AI agents on stage \"{stage_name}\".\n\nResults:\n{}\n\n\
         Combine into one document. Capture best insights, remove redundancy, preserve facts.",
        formatted.join("\n\n"),
    );
    drain(model, model_name, prompt, 4096, session_id, cancel).await
}

async fn drain(
    model: &dyn Model,
    model_name: &str,
    prompt: String,
    max_tokens: u32,
    session_id: Option<&str>,
    cancel: &CancellationToken,
) -> Option<String> {
    let blocks = vec![PromptBlock {
        role: BlockRole::System,
        content: "You are a strict judge. Output only the requested text, nothing else.".into(),
        cache_strategy: None,
        name: Some("team.judge".into()),
        origin: base::prompt::BlockOrigin::Kernel,
    }];
    let messages = vec![base::interface::model::ModelMessage {
        role: base::interface::model::MessageRole::User,
        content: vec![base::interface::model::ModelContentBlock::Text { text: prompt }],
    }];
    let params = StreamParams {
        model: model_name.to_string(),
        max_tokens,
        thinking_mode: ThinkingMode::Off,
        effort: None,
        cache_edits: vec![],
        origin: session_id.map(|id| {
            base::interface::model::CallOrigin::auxiliary(
                id,
                base::interface::model::call_purpose::TEAM_JUDGE,
            )
        }),
        input_map: None,
        provider_search: None,
    };
    let mut stream = model
        .stream(blocks, vec![], messages, params, cancel.clone())
        .await
        .ok()?;
    let mut text = String::new();
    loop {
        let ev = tokio::select! {
            _ = cancel.cancelled() => return None,
            ev = stream.next() => ev,
        };
        let Some(ev) = ev else { break };
        match ev.ok()? {
            ModelEvent::TextDelta { text: t } => text.push_str(&t),
            ModelEvent::EndTurn { .. } => break,
            _ => {}
        }
    }
    if text.trim().is_empty() {
        None
    } else {
        Some(text)
    }
}
