//! Wall-clock readings shared by every crate: epoch milliseconds and RFC 3339.

pub fn unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub fn now_rfc3339() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "unknown".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_parses_back() {
        let s = now_rfc3339();
        assert!(
            time::OffsetDateTime::parse(&s, &time::format_description::well_known::Rfc3339).is_ok()
        );
        assert!(unix_ms() > 1_600_000_000_000);
    }
}
