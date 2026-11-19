//! Provider token quotas as the router reads them: a provider's usage in the
//! day, week and month a call falls in, and where that usage is kept.

use serde::{Deserialize, Serialize};

use super::Quota;

/// One period's usage against its limit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PeriodUsage {
    /// The period's first day, `YYYY-MM-DD`, local time.
    pub start: String,
    pub used: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<u64>,
}

impl PeriodUsage {
    fn used_up(&self) -> bool {
        self.limit.is_some_and(|limit| self.used >= limit)
    }
}

/// A provider's usage in the periods it is in now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct QuotaStatus {
    pub day: PeriodUsage,
    pub week: PeriodUsage,
    pub month: PeriodUsage,
}

impl QuotaStatus {
    /// Why no more calls go to the provider this period, if none do.
    pub fn exhausted(&self) -> Option<String> {
        [
            ("day", &self.day),
            ("week", &self.week),
            ("month", &self.month),
        ]
        .into_iter()
        .find(|(_, usage)| usage.used_up())
        .map(|(period, usage)| {
            format!(
                "{period} quota of {} tokens used up ({} since {})",
                usage.limit.unwrap_or_default(),
                usage.used,
                usage.start
            )
        })
    }
}

/// Where providers' usage is kept.
pub trait QuotaLedger: Send + Sync {
    fn status(&self, provider: &str, quota: &Quota) -> QuotaStatus;
    fn record(&self, provider: &str, tokens: u64);
}
