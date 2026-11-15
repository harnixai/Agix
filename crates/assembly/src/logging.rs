//! The daemon's log filter: `RUST_LOG` with the targets it must not be able
//! to open up by accident: rmcp's OAuth module logs authorization codes and
//! tokens at `debug`, and its service loop logs the answers a host filled
//! into a server's form.

use tracing_subscriber::EnvFilter;

const GUARDED_TARGETS: [&str; 2] = ["rmcp::transport::auth", "rmcp::service"];

pub fn log_filter(rust_log: Option<&str>) -> EnvFilter {
    let spec = rust_log.filter(|s| !s.trim().is_empty()).unwrap_or("info");
    let held_at_info = |base: &str| {
        GUARDED_TARGETS
            .iter()
            .filter(|target| !base.contains(*target))
            .fold(base.to_owned(), |spec, target| {
                format!("{spec},{target}=info")
            })
    };
    EnvFilter::try_new(held_at_info(spec)).unwrap_or_else(|_| EnvFilter::new(held_at_info("info")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing::level_filters::LevelFilter;

    #[test]
    fn debug_everywhere_still_keeps_the_oauth_target_at_info() {
        let f = log_filter(Some("debug")).to_string();
        assert!(f.contains("rmcp::transport::auth=info"), "{f}");
        assert!(f.contains("debug"), "{f}");
    }

    #[test]
    fn debug_everywhere_keeps_the_rmcp_service_loop_at_info() {
        let f = log_filter(Some("debug")).to_string();
        assert!(f.contains("rmcp::service=info"), "{f}");
    }

    #[test]
    fn an_explicit_directive_for_the_target_is_respected() {
        let f = log_filter(Some("rmcp::transport::auth=debug")).to_string();
        assert!(f.contains("rmcp::transport::auth=debug"), "{f}");
        assert!(!f.contains("auth=info"), "{f}");
    }

    #[test]
    fn unset_means_info() {
        assert_eq!(log_filter(None).max_level_hint(), Some(LevelFilter::INFO));
    }
}
