//! What a session and its goal have spent, in the session log: the engine's
//! record of it (`agix.spend`), and reading it back when the session is
//! restored.

use crate::agent::Agent;
use base::context::{Session, SpendRecord};

impl Agent {
    /// Write what the session and its goal have spent, when it changed since
    /// the log last had it.
    pub(crate) async fn record_session_spend(&mut self) -> Result<(), String> {
        let Some(record) = self.session_state.ledger().spend_unrecorded() else {
            return Ok(());
        };
        let written = match serde_json::to_value(&record) {
            Ok(payload) => self
                .session
                .record_extension(base::context::SPEND_NS, base::context::SPEND_EVENT, payload)
                .await
                .map_err(|e| e.to_string()),
            Err(e) => Err(e.to_string()),
        };
        match written {
            Ok(()) => {
                self.session_state.ledger().spent_recorded = Some(record);
                Ok(())
            }
            Err(e) => {
                tracing::warn!(error = %e, "failed to record what the session spent");
                Err(format!("the session's spend: {e}"))
            }
        }
    }
}

/// What `entries`, a session's log, recorded the session and its goal had
/// spent — see [`SpendRecord::latest`]. A rewind does not take a record out:
/// what was spent stays spent.
pub fn recorded_spend(entries: &[history::entry::EnvelopedEntry]) -> Option<SpendRecord> {
    SpendRecord::latest(entries.iter().filter_map(|env| match &env.entry {
        history::entry::LogEntry::Extension { ns, event, payload }
            if ns == base::context::SPEND_NS && event == base::context::SPEND_EVENT =>
        {
            serde_json::from_value(payload.clone()).ok()
        }
        _ => None,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use base::context::GoalSpendRecord;
    use base::interface::budget_policy::Spend;

    fn spent(input: u64, output: u64) -> Spend {
        Spend {
            input_tokens: input,
            output_tokens: output,
            ..Default::default()
        }
    }

    #[test]
    fn the_recorded_spend_is_the_largest_of_its_records() {
        let sid = base::session::SessionId::new();
        let record = |session: Spend, goal: Option<(&str, Spend)>| {
            let record = SpendRecord {
                session,
                goal: goal.map(|(id, spent)| GoalSpendRecord {
                    id: id.into(),
                    spent,
                }),
            };
            history::entry::EnvelopedEntry::new(
                sid,
                history::entry::LogEntry::Extension {
                    ns: base::context::SPEND_NS.into(),
                    event: base::context::SPEND_EVENT.into(),
                    payload: serde_json::to_value(record).unwrap(),
                },
            )
        };
        let entries = [
            record(spent(10, 5), Some(("g1", spent(4, 0)))),
            record(spent(30, 7), Some(("g1", spent(9, 0)))),
            record(spent(20, 9), Some(("g1", spent(6, 0)))),
        ];
        let latest = recorded_spend(&entries).unwrap();
        assert_eq!(latest.session, spent(30, 9));
        assert_eq!(latest.goal.unwrap().spent, spent(9, 0));

        let replaced = [
            record(spent(10, 0), Some(("g1", spent(9, 0)))),
            record(spent(11, 0), Some(("g2", spent(1, 0)))),
        ];
        let latest = recorded_spend(&replaced).unwrap();
        assert_eq!(latest.goal.unwrap().id, "g2");
        assert_eq!(recorded_spend(&[]), None);
    }
}
