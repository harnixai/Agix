//! A provider set's quota ledger, written off the thread that polls the call:
//! a write can wait seconds on another instance's lock. Usage recorded but
//! not yet written counts in [`QuotaKeeper::status`] meanwhile, so the next
//! call sees it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use base::provider::{Quota, QuotaLedger, QuotaStatus};

use super::lock;

/// How many times a write of usage is tried before it is given up on. Each
/// try already waits for the ledger's lock.
const WRITE_ATTEMPTS: u32 = 3;

pub(super) struct QuotaKeeper {
    ledger: Arc<dyn QuotaLedger>,
    unwritten: Arc<Mutex<HashMap<String, u64>>>,
}

impl QuotaKeeper {
    pub(super) fn new(ledger: Arc<dyn QuotaLedger>) -> Self {
        Self {
            ledger,
            unwritten: Arc::default(),
        }
    }

    pub(super) fn status(&self, provider: &str, quota: &Quota) -> QuotaStatus {
        let mut status = self.ledger.status(provider, quota);
        let unwritten = lock(&self.unwritten).get(provider).copied().unwrap_or(0);
        for period in [&mut status.day, &mut status.week, &mut status.month] {
            period.used = period.used.saturating_add(unwritten);
        }
        status
    }

    pub(super) fn record(&self, provider: &str, tokens: u64) {
        if tokens == 0 {
            return;
        }
        *lock(&self.unwritten)
            .entry(provider.to_string())
            .or_default() += tokens;
        let (ledger, unwritten, provider) = (
            self.ledger.clone(),
            self.unwritten.clone(),
            provider.to_string(),
        );
        let write = move || {
            let mut failed = None;
            for _ in 0..WRITE_ATTEMPTS {
                match ledger.record(&provider, tokens) {
                    Ok(()) => {
                        failed = None;
                        break;
                    }
                    Err(error) => failed = Some(error),
                }
            }
            let mut unwritten = lock(&unwritten);
            if let Some(left) = unwritten.get_mut(&provider) {
                *left = left.saturating_sub(tokens);
                if *left == 0 {
                    unwritten.remove(&provider);
                }
            }
            if let Some(error) = failed {
                tracing::warn!(provider, tokens, %error, "quota usage not recorded");
            }
        };
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => drop(runtime.spawn_blocking(write)),
            Err(_) => drop(std::thread::spawn(write)),
        }
    }
}
