//! Compaction aims at, and reports in, the counter the session was built with.

use super::*;
use base::interface::model::{MessageRole, ModelContentBlock, ModelMessage};
use base::interface::token_counter::{default_counter, TokenCounter};

struct Doubled(Arc<dyn TokenCounter>);

impl TokenCounter for Doubled {
    fn count_text(&self, text: &str) -> usize {
        self.0.count_text(text) * 2
    }

    fn count_block(&self, block: &ModelContentBlock) -> usize {
        self.0.count_block(block) * 2
    }
}

fn conversation(agent: &mut Agent, rounds: usize) {
    agent.session.push_message(ModelMessage {
        role: MessageRole::User,
        content: vec![ModelContentBlock::Text {
            text: "go through the files".into(),
        }],
    });
    for i in 0..rounds {
        agent.session.push_message(ModelMessage {
            role: MessageRole::Assistant,
            content: vec![ModelContentBlock::ToolUse {
                id: format!("t{i}"),
                name: "Bash".into(),
                input: serde_json::json!({ "command": format!("cat /f{i}") }),
            }],
        });
        agent.session.push_message(ModelMessage {
            role: MessageRole::User,
            content: vec![ModelContentBlock::ToolResult {
                tool_use_id: format!("t{i}"),
                content: format!("file {i}: {}", "word ".repeat(400)),
                is_error: Some(false),
            }],
        });
    }
}

#[tokio::test]
async fn a_manual_compaction_lands_under_its_target_in_the_sessions_counter() {
    let (mut agent, _rx, _tx) = Builder::new()
        .scene(Arc::new(scene::code::spec(
            scene::code::Interaction::Interactive,
        )))
        .model(Arc::new(FailingModel))
        .settings(Arc::new(test_settings()))
        .token_counter(Arc::new(Doubled(default_counter())))
        .skip_warmup(true)
        .build()
        .expect("build should succeed");
    conversation(&mut agent, 12);

    let before = agent.session.token_count();
    let target = (before / 2).max(1);
    let summary = agent
        .compact_now(&CancellationToken::new())
        .await
        .expect("a compaction");

    assert!(
        agent.session.token_count() <= target,
        "{} tokens left, aimed at {target}",
        agent.session.token_count()
    );
    assert_eq!(summary.tokens_before, before);
    assert_eq!(summary.tokens_after, agent.session.token_count());
}

fn built_with_cached_microcompact(on: bool) -> Agent {
    let mut settings = test_settings();
    settings.values.compaction.clear_on_cache_expiry = on;
    Builder::new()
        .scene(Arc::new(scene::code::spec(
            scene::code::Interaction::Interactive,
        )))
        .model(Arc::new(FailingModel))
        .settings(Arc::new(settings))
        .skip_warmup(true)
        .build()
        .expect("build should succeed")
        .0
}

fn cleared_by_a_pass(agent: &mut Agent) -> usize {
    conversation(agent, 30);
    let mut messages = agent.session.messages().to_vec();
    agent.compaction.cached_mc.run(&mut messages)
}

#[tokio::test]
async fn the_cached_microcompact_setting_reaches_the_pass_it_gates() {
    assert_eq!(
        cleared_by_a_pass(&mut built_with_cached_microcompact(true)),
        10
    );
    assert_eq!(
        cleared_by_a_pass(&mut built_with_cached_microcompact(false)),
        0
    );
}
