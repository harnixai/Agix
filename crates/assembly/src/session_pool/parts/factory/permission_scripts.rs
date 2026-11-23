//! 会话的有效权限模式、权限对象的解析，以及脚本绑定。

use crate::session_pool::*;
use crate::settings_sections::ScriptBindings;

/// The permission mode a session will actually run under.
///
/// The caller's mode — `session.setPermissionMode`'s, else
/// `options.settings.permissions.mode` — is honored, but **clamped** to the
/// daemon's configured `permissions.mode`: a client may tighten the
/// mode for its own session and may not loosen it, since there is no
/// per-method authorization on this socket (see `server/mod.rs`).
///
/// `permissions.client_may_loosen` (default `false`) lets the
/// client win, for deployments where every client is as trusted as the
/// daemon — but never past the mode the managed layer sets.
///
/// With nothing configured anywhere the result is `PermissionMode::Default`
/// — i.e. *ask*. Managed settings that disable bypass turn a
/// `bypassPermissions` result, however it was reached, into `Default`.
pub(in crate::session_pool) fn effective_permission_mode(
    settings: &settings::LoadedSettings,
    requested: Option<base::permission::PermissionMode>,
) -> base::permission::PermissionMode {
    let mode = clamped_permission_mode(&settings.values, requested);
    let held = settings.hold_mode_to_managed(mode);
    let bypass_off = mode == base::permission::PermissionMode::BypassPermissions
        && held == base::permission::PermissionMode::Default
        && settings.bypass_disabled();
    if bypass_off {
        warn!("the managed settings disable bypassPermissions; this session runs in default mode");
    }
    held
}

/// The session's configuration over `base`: its own settings `layer` laid
/// over it, and the permission mode — `requested` clamped against `base` — it
/// runs in; with the keys of `layer` held to the settings' value, as fields of
/// `options`. The mode is decided by [`effective_permission_mode`] apart from
/// the layer, so the managed layer has the last word over it.
pub(in crate::session_pool) fn session_settings(
    base: &Arc<settings::LoadedSettings>,
    layer: Option<&serde_json::Map<String, serde_json::Value>>,
    requested: Option<base::permission::PermissionMode>,
) -> (settings::SessionConfig, Vec<api::ClampedOption>) {
    let mode = effective_permission_mode(base, requested);
    let config = settings::SessionConfig::with_session_layer(
        base.clone(),
        layer,
        &crate::settings_sections::all(),
    )
    .with_permission_mode(mode);
    let mut clamped: Vec<api::ClampedOption> = config
        .held()
        .iter()
        .filter(|held| held.key != "permissions.mode")
        .map(|held| api::ClampedOption {
            field: format!("settings.{}", held.key),
            requested: held.requested.clone(),
            applied: held.applied.clone(),
        })
        .collect();
    let asked = layer.and_then(api::requested_permission_mode);
    if let Some(asked) = asked.filter(|asked| requested == Some(*asked) && mode != *asked) {
        clamped.insert(
            0,
            api::ClampedOption {
                field: "settings.permissions.mode".into(),
                requested: serde_json::to_value(asked).unwrap_or_default(),
                applied: serde_json::to_value(mode).unwrap_or_default(),
            },
        );
    }
    (config, clamped)
}

/// Those of `keys`, dotted settings keys, that `base` holds `spec`'s own
/// settings off.
pub(in crate::session_pool) fn held_keys(
    base: &Arc<settings::LoadedSettings>,
    spec: &SessionSpec,
    keys: &[&str],
) -> Vec<api::ClampedOption> {
    let (_, held) = session_settings(
        base,
        spec.settings_layer(),
        spec.requested_permission_mode(),
    );
    held.into_iter()
        .filter(|h| {
            h.field
                .strip_prefix("settings.")
                .is_some_and(|key| keys.contains(&key))
        })
        .collect()
}

/// The reasoning `spec`'s session runs on over `base`, each value marked
/// `session` when its own settings set it and `base` did not hold it off.
pub(in crate::session_pool) fn reasoning_report(
    base: &Arc<settings::LoadedSettings>,
    spec: &SessionSpec,
) -> runtime::agent::ReasoningReport {
    let (laid, held) = session_settings(
        base,
        spec.settings_layer(),
        spec.requested_permission_mode(),
    );
    let mut report = runtime::agent::ReasoningReport::of(&laid.settings().values.model);
    let own = |key: &str| {
        spec.session_setting(key).is_some()
            && !held
                .iter()
                .any(|h| h.field.strip_prefix("settings.") == Some(key))
    };
    if own("model.effort") {
        report.effort_source = runtime::agent::ChoiceSource::Session;
    }
    if own("model.thinking") {
        report.thinking_source = runtime::agent::ChoiceSource::Session;
    }
    report
}

fn clamped_permission_mode(
    settings: &Settings,
    requested: Option<base::permission::PermissionMode>,
) -> base::permission::PermissionMode {
    let Some(requested) = requested else {
        return settings.permissions.mode;
    };
    if settings.permissions.client_may_loosen {
        return requested;
    }
    let clamped = requested.min_permissive(settings.permissions.mode);
    if clamped != requested {
        warn!(
            requested = ?requested,
            configured = ?settings.permissions.mode,
            effective = ?clamped,
            "session requested a more permissive mode than this daemon allows; clamped \
             (set `permissions.client_may_loosen: true` to permit it)"
        );
    }
    clamped
}

/// Turn `settings.scripts` into a prompt registry, or nothing.
///
/// `None` when no scripts are configured, which is the overwhelming case and
/// costs the session nothing — the builder then uses its own default and no
/// registry is allocated.
///
/// A binding that cannot be honored is logged and the whole set is dropped,
/// rather than half of it being applied. That is louder than it looks on
/// purpose: a script that silently never runs sends its author looking for a
/// bug in their JavaScript.
#[cfg(feature = "scripts")]
pub(in crate::session_pool) fn bind_scripts(
    settings: &Arc<settings::LoadedSettings>,
    project_root: &std::path::Path,
    packages: &[crate::plugins::PackageScripts],
) -> (
    Option<script_host::bindings::BoundScripts>,
    Vec<(String, String)>,
) {
    use script_host::bindings::{bind_into, bind_lenient, BindingSource, BoundScripts};

    if ScriptBindings::of(&settings.values).is_empty() && packages.is_empty() {
        return (None, Vec::new());
    }

    // One value, so one ledger: the operator's scripts and the packages'
    // share the record a turn resets and `session.scripts` reads. Two would
    // have left whichever the session did not hold running on a per-turn
    // budget nothing ever reset.
    let mut bound = BoundScripts::default();
    let engine = script_host::bindings::engine_for(settings.paths.layout.engine());

    if let Err(e) = bind_into(
        &mut bound,
        engine.clone(),
        &operator_script_bindings(settings),
        &BindingSource::Project { root: project_root },
    ) {
        // All-or-nothing, and `bind_into` already left `bound` untouched.
        // Louder than it looks on purpose: a script that silently never runs
        // sends its author looking for a bug in their JavaScript.
        error!(error = %e, "a script binding is invalid; no scripts from settings were bound");
    }

    let mut faults = Vec::new();
    for package in packages {
        for e in bind_lenient(
            &mut bound,
            engine.clone(),
            &package.bindings,
            &BindingSource::Package {
                name: &package.name,
                root: &package.root,
            },
        ) {
            // One binding at a time, and the failure is recorded against the
            // package that shipped it: a set assembled from several packages
            // has no single author, so dropping it whole would let one bad
            // package take away every other package's contributions.
            warn!(plugin = %package.name, error = %e, "a plugin script binding was not honored");
            faults.push((package.name.clone(), e.to_string()));
        }
    }

    if bound.carriers.is_empty() {
        return (None, faults);
    }
    info!(
        scripts = bound.carriers.len(),
        "bound scripts to extension points"
    );
    (Some(bound), faults)
}

/// `settings.scripts`, less — while the project is not trusted — each binding
/// with a relative path. The project's own bindings are already withheld, so
/// these come from the global or scene tier, and a relative path there would
/// load whatever file the repository puts at it, with the authority of a
/// script inside the project.
#[cfg_attr(not(feature = "scripts"), allow(dead_code))]
pub(in crate::session_pool) fn operator_script_bindings(
    settings: &settings::LoadedSettings,
) -> Vec<base::interface::script::ScriptBinding> {
    let bindings = ScriptBindings::of(&settings.values);
    if !settings.trust().is_untrusted() {
        return bindings;
    }
    bindings
        .iter()
        .filter(|binding| {
            let absolute = binding.path.is_absolute();
            if !absolute {
                warn!(
                    path = %binding.path.display(),
                    point = %binding.point,
                    "the project is not trusted; a script binding with a relative path, which \
                     would load a file from it, is not bound until it is"
                );
            }
            absolute
        })
        .cloned()
        .collect()
}

/// Without the script carrier compiled in, a `scripts` section is refused
/// loudly rather than ignored — and so is a package that ships one.
#[cfg(not(feature = "scripts"))]
pub(in crate::session_pool) fn bind_scripts(
    settings: &Arc<settings::LoadedSettings>,
    _project_root: &std::path::Path,
    packages: &[crate::plugins::PackageScripts],
) -> (Option<()>, Vec<(String, String)>) {
    const NO_ENGINE: &str = "this build carries no script engine";

    let scripts = ScriptBindings::of(&settings.values);
    if !scripts.is_empty() {
        error!(
            scripts = scripts.len(),
            "this build carries no script engine, so a `scripts` section cannot be honored; \
             rebuild with the `scripts` feature or remove the section"
        );
    }
    let faults = packages
        .iter()
        .flat_map(|p| {
            p.bindings
                .iter()
                .map(|b| {
                    (
                        p.name.clone(),
                        format!("{NO_ENGINE}, so `{}` is not bound", b.point),
                    )
                })
                .collect::<Vec<_>>()
        })
        .collect();
    (None, faults)
}

/// Which permission a new session gets: a `Policy` over every tier's rules
/// — its `options.settings` among them — and the host's policy, keeping its
/// own grants.
pub(in crate::session_pool) fn resolve_session_permission(
    host: Option<&Arc<dyn PermissionPolicy>>,
    settings: &settings::LoadedSettings,
) -> PermissionSet {
    PermissionSet::of(Arc::new(
        permissions::policy::Policy::from_settings(&settings.permission_settings(), Vec::new())
            .with_host(host.cloned()),
    ))
}
