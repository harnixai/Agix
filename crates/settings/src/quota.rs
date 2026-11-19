//! Provider token quotas kept on disk: the day, week and month a call falls
//! in, by this machine's local time, and each provider's usage in a file of
//! its own under the state root's `quota/`, shared by every instance on the
//! machine.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use base::provider::{PeriodUsage, Quota, QuotaLedger, QuotaStatus};
use serde::{Deserialize, Serialize};
use time::{Date, Duration, OffsetDateTime, UtcOffset};

/// What time it is, in the offset the periods are counted in.
pub trait QuotaClock: Send + Sync {
    fn now(&self) -> OffsetDateTime;
}

/// This machine's local time.
pub struct LocalClock;

impl QuotaClock for LocalClock {
    fn now(&self) -> OffsetDateTime {
        let utc = OffsetDateTime::now_utc();
        utc.to_offset(local_offset_at(utc.unix_timestamp()))
    }
}

/// `time` refuses the local offset in a multi-threaded process; the C
/// library's reentrant call is safe there.
#[cfg(unix)]
fn local_offset_at(timestamp: i64) -> UtcOffset {
    let t = timestamp as libc::time_t;
    // SAFETY: `tm` is plain data; `localtime_r` writes only into it.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: both pointers are valid for the duration of the call.
    if unsafe { libc::localtime_r(&t, &mut tm) }.is_null() {
        return UtcOffset::UTC;
    }
    UtcOffset::from_whole_seconds(tm.tm_gmtoff as i32).unwrap_or(UtcOffset::UTC)
}

#[cfg(not(unix))]
fn local_offset_at(_timestamp: i64) -> UtcOffset {
    UtcOffset::UTC
}

/// The first day of the day, week and month `now` falls in. Kept as dates,
/// not instants, so a change of offset within a period does not start a new
/// one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeriodStarts {
    pub day: Date,
    pub week: Date,
    pub month: Date,
}

pub fn period_starts(now: OffsetDateTime) -> PeriodStarts {
    let day = now.date();
    let week = day - Duration::days(i64::from(day.weekday().number_days_from_monday()));
    let month = day.replace_day(1).unwrap_or(day);
    PeriodStarts { day, week, month }
}

/// Usage in `<dir>/<provider>.json`, read and rewritten under a lock on
/// `dir`.
pub struct FileQuotaLedger {
    dir: PathBuf,
    clock: Arc<dyn QuotaClock>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct UsageFile {
    #[serde(default)]
    day: Option<Counted>,
    #[serde(default)]
    week: Option<Counted>,
    #[serde(default)]
    month: Option<Counted>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Counted {
    start: String,
    used: u64,
}

fn used_in(counted: &Option<Counted>, start: &str) -> u64 {
    counted
        .as_ref()
        .filter(|c| c.start == start)
        .map_or(0, |c| c.used)
}

fn add_to(counted: &mut Option<Counted>, start: String, tokens: u64) {
    let used = used_in(counted, &start).saturating_add(tokens);
    *counted = Some(Counted { start, used });
}

impl FileQuotaLedger {
    pub fn new(dir: PathBuf) -> Self {
        Self::with_clock(dir, Arc::new(LocalClock))
    }

    pub fn with_clock(dir: PathBuf, clock: Arc<dyn QuotaClock>) -> Self {
        Self { dir, clock }
    }

    fn file(&self, provider: &str) -> PathBuf {
        let name: String = provider
            .chars()
            .map(|c| match c {
                'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' => c,
                _ => '_',
            })
            .collect();
        self.dir.join(format!("{name}.json"))
    }

    fn read(path: &Path) -> UsageFile {
        std::fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    fn starts(&self) -> [String; 3] {
        let starts = period_starts(self.clock.now());
        [starts.day, starts.week, starts.month].map(|date| date.to_string())
    }

    fn write(path: &Path, usage: &UsageFile) -> std::io::Result<()> {
        let bytes = serde_json::to_vec_pretty(usage).map_err(std::io::Error::other)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, path)
    }
}

impl QuotaLedger for FileQuotaLedger {
    fn status(&self, provider: &str, quota: &Quota) -> QuotaStatus {
        let usage = Self::read(&self.file(provider));
        let [day, week, month] = self.starts();
        QuotaStatus {
            day: PeriodUsage {
                used: used_in(&usage.day, &day),
                start: day,
                limit: quota.day,
            },
            week: PeriodUsage {
                used: used_in(&usage.week, &week),
                start: week,
                limit: quota.week,
            },
            month: PeriodUsage {
                used: used_in(&usage.month, &month),
                start: month,
                limit: quota.month,
            },
        }
    }

    fn record(&self, provider: &str, tokens: u64) {
        if tokens == 0 {
            return;
        }
        let _held = match base::dir_lock::DirLock::acquire_within(
            &self.dir,
            base::dir_lock::DIR_LOCK_WAIT,
        ) {
            Ok(held) => held,
            Err(error) => {
                tracing::warn!(provider, %error, "quota usage not recorded: its directory could not be locked");
                return;
            }
        };
        let path = self.file(provider);
        let mut usage = Self::read(&path);
        let [day, week, month] = self.starts();
        add_to(&mut usage.day, day, tokens);
        add_to(&mut usage.week, week, tokens);
        add_to(&mut usage.month, month, tokens);
        if let Err(error) = Self::write(&path, &usage) {
            tracing::warn!(provider, path = %path.display(), %error, "quota usage not recorded");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use time::macros::datetime;

    #[test]
    fn periods_start_at_local_midnight_monday_and_the_first() {
        let starts = period_starts(datetime!(2026-11-19 23:59:59 +08:00));
        assert_eq!(starts.day.to_string(), "2026-11-19");
        assert_eq!(starts.week.to_string(), "2026-11-16");
        assert_eq!(starts.month.to_string(), "2026-11-01");

        let monday = period_starts(datetime!(2026-11-23 00:00:00 +08:00));
        assert_eq!(monday.week.to_string(), "2026-11-23");
        let sunday = period_starts(datetime!(2026-11-22 23:59:59 +08:00));
        assert_eq!(sunday.week.to_string(), "2026-11-16");

        let new_month = period_starts(datetime!(2026-12-01 00:00:00 -05:00));
        assert_eq!(new_month.month.to_string(), "2026-12-01");
        assert_eq!(new_month.week.to_string(), "2026-11-30");
    }

    struct FixedClock(Mutex<OffsetDateTime>);

    impl QuotaClock for FixedClock {
        fn now(&self) -> OffsetDateTime {
            *self.0.lock().unwrap()
        }
    }

    #[test]
    fn usage_starts_over_when_its_period_turns_over() {
        let dir = tempfile::tempdir().unwrap();
        let clock = Arc::new(FixedClock(Mutex::new(datetime!(2026-11-30 23:00 +00:00))));
        let ledger = FileQuotaLedger::with_clock(dir.path().join("quota"), clock.clone());
        let quota = Quota {
            day: Some(100),
            week: Some(1000),
            month: None,
        };
        ledger.record("ds", 60);
        ledger.record("ds", 40);
        let status = ledger.status("ds", &quota);
        assert_eq!(status.day.used, 100);
        assert!(status.exhausted().unwrap().contains("day"));

        *clock.0.lock().unwrap() = datetime!(2026-12-01 00:00 +00:00);
        let status = ledger.status("ds", &quota);
        assert_eq!(status.day.used, 0, "a new day");
        assert_eq!(status.week.used, 100, "the same week");
        assert_eq!(status.month.used, 0, "a new month");
        assert_eq!(status.exhausted(), None);
        assert_eq!(ledger.status("other", &quota).day.used, 0);
    }
}
