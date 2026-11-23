//! Built-in slash commands handled by the agent itself (`/help`, `/skills`, `/clear`, `/cost`).

use super::*;
use base::context::Session;

impl Agent {
    /// `/help` — list all available slash commands.
    pub(crate) fn handle_help_command(&self) -> String {
        let commands = self.tooling.commands().list();
        if commands.is_empty() {
            return "No commands registered.".into();
        }
        let mut out = String::from("Available slash commands:\n\n");
        for (name, desc) in &commands {
            out.push_str(&format!("/{} — {}\n", name, desc));
        }
        out.push_str("\nUse /<name> [args] to invoke a command.");
        out
    }

    /// `/skills` — list all available skills.
    pub(crate) fn handle_skills_command(&self) -> String {
        let skills = self.tooling.skills().list();
        if skills.is_empty() {
            return "No skills loaded.".into();
        }
        let mut out = String::from("Available skills:\n\n");
        for s in &skills {
            let src = s.source.as_str();
            out.push_str(&format!("/{} — {} [{}]\n", s.name, s.description, src));
        }
        out
    }

    /// `/clear` — clear session message history.
    pub(crate) async fn handle_clear_command(&mut self) {
        if let Err(e) = self.session.clear().await {
            tracing::warn!(error = %e, "failed to record /clear in the transcript");
        }
        self.prompt.forget_conversation();
        self.gate.reset_denials();
        let mut ledger = self.session_state.ledger();
        ledger.invoked_skills.clear();
        ledger.token_target.clear();
    }

    /// `/cost` — report the tokens and calls this session has spent.
    pub(crate) fn handle_cost_command(&self) -> String {
        let (spent, calls) = {
            let ledger = self.session_state.ledger();
            (ledger.spent.read(), ledger.calls)
        };
        format!(
            "Since this session was created: {} input tokens, {} output tokens, {} written to \
             and {} read from the prompt cache, as the provider reported them, its sub-agents, \
             team members and the calls made on the side included. Since it was loaded, its own \
             agent made {} model calls and {} tool calls.",
            spent.input_tokens,
            spent.output_tokens,
            spent.cache_creation_tokens,
            spent.cache_read_tokens,
            calls.api_calls,
            calls.tool_calls,
        )
    }
}
