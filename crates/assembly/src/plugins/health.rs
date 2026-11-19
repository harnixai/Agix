//! What `daemon.doctor` reports about the package subsystem: the enable
//! state files, the packages discovery refuses, and the install default.

use base::interface::health::{CheckResult, HealthCheck};

/// `daemon.doctor`'s view of the two `enabled.json` files: a tier whose
/// file cannot be read holds every package in it disabled, and nothing
/// else would say why.
pub(super) struct EnableStateCheck(pub(super) (std::path::PathBuf, std::path::PathBuf));

impl HealthCheck for EnableStateCheck {
    fn name(&self) -> &str {
        "plugins.enable_state"
    }

    fn check(&self) -> CheckResult {
        let (global, scene) = &self.0;
        let faults: Vec<serde_json::Value> = [("global", global), ("scene", scene)]
            .into_iter()
            .filter_map(|(tier, root)| {
                ::packages::state::EnableState::new(root.clone())
                    .fault()
                    .map(|fault| serde_json::json!({"tier": tier, "fault": fault}))
            })
            .collect();
        if faults.is_empty() {
            CheckResult::ok("plugin enable state reads")
        } else {
            CheckResult::degraded(
                "an unreadable enabled.json holds every plugin in its tier disabled",
            )
            .with_details(serde_json::json!({"tiers": faults}))
        }
    }
}

/// Packages on disk that discovery sets aside — a disclosure over its
/// limits, a manifest that no longer loads — or that use a library this host
/// does not provide, and so never load, however they got into the cache.
pub(super) struct RefusedPackagesCheck {
    pub(super) tiers: (std::path::PathBuf, std::path::PathBuf),
    pub(super) layout: base::paths::Layout,
    pub(super) libraries: Vec<base::command::HostLibrary>,
}

impl HealthCheck for RefusedPackagesCheck {
    fn name(&self) -> &str {
        "plugins.refused"
    }

    fn check(&self) -> CheckResult {
        let (global, scene) = &self.tiers;
        let found = ::packages::scan_plugins(global, scene, &self.layout);
        let mut refused = found.refused;
        refused.extend(found.plugins.iter().filter_map(|p| {
            ::packages::host::check_libraries(p, &self.libraries)
                .err()
                .map(|e| ::packages::Refused {
                    name: p.name().to_string(),
                    version: p.manifest.plugin.version.clone(),
                    reason: e.to_string(),
                })
        }));
        if refused.is_empty() {
            return CheckResult::ok("every installed plugin loads");
        }
        let details: Vec<serde_json::Value> = refused
            .iter()
            .map(|r| {
                serde_json::json!({
                    "name": r.name,
                    "version": r.version,
                    "reason": r.reason,
                })
            })
            .collect();
        CheckResult::degraded(format!(
            "{} installed plugin(s) are refused on loading and not active: {}",
            refused.len(),
            refused
                .iter()
                .map(|r| r.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ))
        .with_details(serde_json::json!({"refused": details}))
    }
}

/// Says which way `plugins.enable_on_install` is set, and, while it is
/// still `true`, that its default is planned to change.
pub(super) struct EnableOnInstallCheck(pub(super) bool);

impl HealthCheck for EnableOnInstallCheck {
    fn name(&self) -> &str {
        "plugins.enable_on_install"
    }

    fn check(&self) -> CheckResult {
        let details = serde_json::json!({"enable_on_install": self.0});
        if self.0 {
            CheckResult::ok(
                "plugin.install enables a package as soon as it is installed; \
                 plugins.enable_on_install is planned to default to false in a later \
                 0.3.x release, after which the host must call plugin.enable",
            )
        } else {
            CheckResult::ok(
                "plugin.install leaves a package disabled until the host calls plugin.enable",
            )
        }
        .with_details(details)
    }
}
