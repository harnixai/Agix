//! What loading the settings tiers produced: the merged [`Settings`], where
//! this scene's data lives, every permission rule with the tier it came from,
//! the deployment's managed layer, and what was decided about the project
//! tier. A settings file can set none of these but the values.
//!
//! [`LoadedSettings::load`] is the single canonical settings.json loader —
//! `daemon` and any other embedder call it rather than parsing settings.json
//! themselves. [`LoadedSettings::load_from`] takes the layers from any
//! [`ConfigSource`](base::interface::config_source::ConfigSource) and runs the
//! same merge.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use base::interface::config_source::{ConfigSource, Tier};
use base::paths::{Layout, PathSettings, Workspace};
use base::permission::{LoadedRule, ManagedSettings, PermissionMode, ProjectTrust, RuleBehavior};

use crate::merge::{merge_fitting, merge_json_values};
use crate::permissions::{self, read_rule_list, take_rule_lists};
use crate::section::Sections;
use crate::trust::{self, ProjectLayer, ProjectTrustSettings};
use crate::{unknown_settings_keys, FileTiers, IgnoredValue, Settings};

#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct LoadedSettings {
    pub values: Settings,
    pub paths: PathSettings,
    /// Every tier's `permissions` rule lists, lowest tier first. They are
    /// concatenated, never overridden: a project's rules are added to the
    /// global and scene ones.
    pub rules: Vec<LoadedRule>,
    /// The deployment's layer, above every other — see
    /// [`crate::managed`].
    pub managed: Option<ManagedSettings>,
    /// For each of `values.mcp_servers`, the directory of the tier its
    /// `command` was declared in: the global or scene configuration
    /// directory, or the project root. A stdio server starts there.
    pub mcp_server_dirs: HashMap<String, PathBuf>,
    /// The values the loader left out because they do not fit their setting.
    pub ignored: Vec<IgnoredValue>,
    project_layer: ProjectLayer,
}

impl LoadedSettings {
    /// Settings that were not loaded from any tier: no rules, no managed
    /// layer, and no project tier of their own.
    pub fn new(values: Settings, paths: PathSettings) -> Self {
        Self {
            values,
            paths,
            rules: Vec::new(),
            managed: None,
            mcp_server_dirs: HashMap::new(),
            ignored: Vec::new(),
            project_layer: ProjectLayer::default(),
        }
    }

    /// Load settings from the three tiers on disk. Priority (low → high):
    /// `global_dir/settings.json` (shared by every scene) →
    /// `scene_dir/settings.json` → `local_dir/settings.json` (project). Each
    /// tier is immediately followed by its own gitignored
    /// `settings.local.json` overlay, which outranks the `settings.json`
    /// beside it. An embedder whose configuration does not live on this
    /// machine's disk calls [`load_from`](Self::load_from) with its own source
    /// and gets the same merge.
    ///
    /// Merging is a **generic recursive JSON merge** (later layer's object
    /// keys override/extend the earlier layer's, non-object values fully
    /// replace) applied directly to the JSON representation, not a
    /// hand-written field-by-field merge — this is why adding a new field to
    /// `Settings` needs no changes here. A file's `paths` is dropped before
    /// merging: a settings.json file cannot override where its own layers
    /// live, that's decided by the `global_dir`/`scene_dir`/`local_dir`/
    /// `scope` arguments alone.
    ///
    /// The project tier is the one exception to "every layer merges the same
    /// way": unless the global tier's `project_trust` trusts the project root
    /// (`local_dir`'s parent), what in it could loosen anything is held back
    /// before it merges — see [`trust`]. `project_trust`
    /// itself is read from the global tier only. The decision, and what it
    /// held back, is [`trust`](Self::trust) and
    /// [`requires_trust`](Self::requires_trust).
    ///
    /// **Never fails** — a layer that doesn't exist is skipped; a layer that
    /// fails to parse is skipped with a `tracing::warn!` (path + error)
    /// rather than aborting startup or silently losing the problem. A value
    /// that parses but does not fit its setting (a string where an object
    /// was expected) is left out by itself, with a warning and an entry in
    /// `ignored`; the rest of its layer and every other layer apply.
    ///
    /// `scope` identifies which product instance `scene_dir` belongs to (see
    /// `base::paths::ConfigPaths`) — no default at this layer; callers decide.
    ///
    /// `default_model` seeds `model.model_name` **before** any layer is
    /// merged in — lower priority than every settings.json layer. This lets
    /// a caller's CLI `--model` flag act as a fallback rather than a hard
    /// override.
    ///
    /// `layout` is what the instance names its directories; `local_dir` is
    /// its project directory in the project, and every session of these
    /// settings carries it, with its worktrees where the global and scene
    /// tiers' `worktree_dir` puts them ([`Self::place_worktrees`]).
    pub fn load(
        global_dir: PathBuf,
        scene_dir: PathBuf,
        local_dir: PathBuf,
        scope: &str,
        default_model: &str,
        sections: &Sections,
        layout: Layout,
    ) -> Self {
        let source = FileTiers::new(global_dir.clone(), scene_dir.clone(), local_dir.clone());
        Self::load_from(
            &source,
            global_dir,
            scene_dir,
            local_dir,
            scope,
            default_model,
            sections,
            layout,
        )
    }

    /// [`load`](Self::load) for a session without a project: no project tier,
    /// and a [`Workspace::Scratch`] in the state root's scratchpad until the
    /// session is given a directory of its own
    /// ([`base::paths::session_scratch_dir`]).
    pub fn load_without_project(
        global_dir: PathBuf,
        scene_dir: PathBuf,
        scope: &str,
        default_model: &str,
        sections: &Sections,
        layout: Layout,
    ) -> Self {
        let scratch = global_dir.join(base::paths::STATE_ROOT_SCRATCHPAD);
        let mut loaded = Self::load(
            global_dir.clone(),
            scene_dir,
            global_dir,
            scope,
            default_model,
            sections,
            layout,
        );
        loaded.paths.workspace = Workspace::Scratch(scratch);
        loaded
    }

    /// [`load`](Self::load) with the layers coming from wherever `source` says
    /// instead of from the six files on disk.
    ///
    /// The merge is identical — that is the point of the split. `source`
    /// decides only which layers exist, in what order, and what JSON is in
    /// them; everything after that happens here and is the same for every
    /// source.
    ///
    /// The directory arguments are not where configuration is read from —
    /// they are where this scene's *data* lives, which `paths` reports and a
    /// settings file is never allowed to override — and they say which
    /// project the project tier is, and whether it is one of its own.
    #[allow(clippy::too_many_arguments)]
    pub fn load_from(
        source: &dyn ConfigSource,
        global_dir: PathBuf,
        scene_dir: PathBuf,
        local_dir: PathBuf,
        scope: &str,
        default_model: &str,
        sections: &Sections,
        layout: Layout,
    ) -> Self {
        let base = Settings::defaults_for(default_model);
        let mut merged_json = serde_json::to_value(&base).unwrap_or_else(|_| serde_json::json!({}));
        seed_section_defaults(&mut merged_json, sections);

        let layers = source.layers();
        let own_project = trust::is_own_project_dir(&local_dir, &global_dir, &scene_dir);
        let tiers = TierView {
            own_project,
            scene_is_global: trust::same_dir(&scene_dir, &global_dir),
            project_is_global: !own_project && trust::same_dir(&local_dir, &global_dir),
        };

        let project_trust = ProjectTrustSettings::from_layers(
            layers
                .iter()
                .filter(|l| tiers.is_global(l.tier))
                .map(|l| (&l.value, l.machine_local)),
        );
        let project_layer =
            project_trust.decide_tier(own_project, &local_dir, &global_dir, &scene_dir);
        let withhold = project_layer.trust.is_untrusted();
        let mut fold = LayerFold {
            sections,
            layout: &layout,
            tiers,
            withhold,
            global_dir: &global_dir,
            scene_dir: &scene_dir,
            local_dir: &local_dir,
            merged_json,
            loaded_rules: Vec::new(),
            project_layer,
            ignored: Vec::new(),
            managed: None,
            mcp_server_dirs: HashMap::new(),
            mcp_secret_vars: Vec::new(),
        };
        for layer in layers {
            fold.apply(layer);
        }
        let LayerFold {
            merged_json,
            loaded_rules,
            mut project_layer,
            ignored,
            managed,
            mcp_server_dirs,
            mcp_secret_vars,
            ..
        } = fold;
        let mut seen = std::collections::HashSet::new();
        project_layer
            .requires_trust
            .retain(|w| seen.insert((w.key.clone(), w.origin.clone())));

        // Every merge step above was checked to deserialize.
        let mut values: Settings = serde_json::from_value(merged_json).unwrap_or(base);
        values.sections.retain(|key, _| sections.get(key).is_some());
        values.project_trust = project_trust;
        guard_loaded_values(&mut values, &mcp_secret_vars);

        let mut paths = PathSettings::new(
            scene_dir,
            global_dir,
            local_dir.clone(),
            Workspace::project_of(&local_dir),
            scope.to_string(),
            layout,
        );
        paths.project_untrusted = withhold;
        let mut loaded = Self {
            values,
            paths,
            rules: loaded_rules,
            managed: None,
            mcp_server_dirs,
            ignored,
            project_layer,
        };
        if let Err(e) = loaded.place_worktrees() {
            tracing::warn!(error = %e, "worktree_dir is not allowed, the worktrees stay where they were");
        }
        loaded.with_managed(managed)
    }

    /// Put the deployment's layer on top of these settings, replacing any
    /// managed layer they already carry. With bypass disabled, a
    /// `bypassPermissions` the lower tiers configured becomes `default`.
    pub fn with_managed(mut self, managed: Option<ManagedSettings>) -> Self {
        if let Some(m) = &managed {
            if m.disable_bypass && self.values.permissions.mode == PermissionMode::BypassPermissions
            {
                tracing::warn!(origin = %m.origin, "managed settings disable bypassPermissions; permissions.mode falls back to default");
                self.values.permissions.mode = PermissionMode::Default;
            }
        }
        self.managed = managed;
        self
    }

    /// These settings as if the loader had decided `layer` about the project
    /// tier.
    pub fn with_project_layer(mut self, layer: ProjectLayer) -> Self {
        self.paths.project_untrusted = layer.trust.is_untrusted();
        self.project_layer = layer;
        self
    }

    /// Put `paths.layout`'s worktrees where `values.worktree_dir` names, the
    /// default when it names none. A name the layout does not allow leaves
    /// both as they were.
    pub fn place_worktrees(&mut self) -> Result<(), base::paths::WorktreesDirError> {
        let name = self
            .values
            .worktree_dir
            .as_deref()
            .unwrap_or(base::paths::DEFAULT_WORKTREES_DIR);
        self.paths.layout = self.paths.layout.clone().with_worktrees_dir(name)?;
        Ok(())
    }

    /// Whether the project tier is in effect as written.
    pub fn trust(&self) -> ProjectTrust {
        self.project_layer.trust
    }

    /// The `project_trust.trusted` pattern that matched the project root;
    /// `None` when the default decided, or there is no project tier.
    pub fn trust_pattern(&self) -> Option<&str> {
        self.project_layer.matched.as_deref()
    }

    /// What decided [`trust`](Self::trust); `None` when there is no project
    /// tier.
    pub fn trust_source(&self) -> Option<trust::TrustSource> {
        self.project_layer.source
    }

    /// The project-tier settings that take effect only while the project is
    /// trusted, as dotted keys (`hooks_config`, `mcp_servers.github`,
    /// `permissions.allow(Bash)`). Withheld while it is not.
    pub fn requires_trust(&self) -> Vec<String> {
        let mut seen = std::collections::HashSet::new();
        self.project_layer
            .requires_trust
            .iter()
            .filter(|w| seen.insert(w.key.as_str()))
            .map(|w| w.key.clone())
            .collect()
    }

    /// [`requires_trust`](Self::requires_trust) with the file each came from.
    pub fn withheld(&self) -> &[trust::Withheld] {
        &self.project_layer.requires_trust
    }

    /// The project tier's own `mcp_servers` entries, both of its files
    /// merged — whether or not they were withheld.
    pub fn project_mcp_servers(&self) -> &BTreeMap<String, serde_json::Value> {
        &self.project_layer.mcp_servers
    }

    /// Whether the deployment has taken `bypassPermissions` away.
    pub fn bypass_disabled(&self) -> bool {
        self.managed.as_ref().is_some_and(|m| m.disable_bypass)
    }
}

/// A section's defaults are in the merge from the start, so a project that is
/// not trusted is held to them as it is to every core default.
fn seed_section_defaults(merged: &mut serde_json::Value, sections: &Sections) {
    for spec in sections.iter() {
        let default = spec.default_value();
        if !default.is_null() {
            merged[spec.key] = default;
        }
    }
}

/// Two tiers sharing a directory are read once, at the higher tier: a project
/// rooted at `$HOME` reads `~/.agix` as its project tier, and those files are
/// still the user's global settings.
struct TierView {
    own_project: bool,
    scene_is_global: bool,
    project_is_global: bool,
}

impl TierView {
    fn is_global(&self, tier: Tier) -> bool {
        match tier {
            Tier::Global => true,
            Tier::Scene => self.scene_is_global,
            Tier::Project => self.project_is_global,
            Tier::Managed => false,
        }
    }

    fn is_own_project(&self, tier: Tier) -> bool {
        tier == Tier::Project && self.own_project
    }
}

struct LayerFold<'a> {
    sections: &'a Sections,
    layout: &'a Layout,
    tiers: TierView,
    withhold: bool,
    global_dir: &'a std::path::Path,
    scene_dir: &'a std::path::Path,
    local_dir: &'a std::path::Path,
    merged_json: serde_json::Value,
    loaded_rules: Vec<LoadedRule>,
    project_layer: ProjectLayer,
    ignored: Vec<IgnoredValue>,
    managed: Option<ManagedSettings>,
    mcp_server_dirs: HashMap<String, PathBuf>,
    mcp_secret_vars: Vec<String>,
}

impl LayerFold<'_> {
    fn apply(&mut self, layer: base::interface::config_source::ConfigLayer) {
        if layer.tier == Tier::Managed {
            let this = crate::managed::from_value(layer.origin, layer.value);
            self.managed = Some(match self.managed.take() {
                Some(earlier) => earlier.followed_by(this),
                None => this,
            });
            return;
        }
        for key in unknown_settings_keys(&layer.value, self.sections) {
            match crate::renamed::renamed_to(&key) {
                Some(renamed_to) => {
                    tracing::warn!(layer = layer.tier.name(), origin = %layer.origin, key, renamed_to, "settings key was renamed, ignoring it")
                }
                None => {
                    tracing::warn!(layer = layer.tier.name(), origin = %layer.origin, key, "settings key not recognised, ignoring it")
                }
            }
        }
        let is_global = self.tiers.is_global(layer.tier);
        let is_own_project = self.tiers.is_own_project(layer.tier);
        let mut layer_json = layer.value;
        if let Some(obj) = layer_json.as_object_mut() {
            obj.remove("paths");
            drop_keys_not_read_from(layer.tier, is_global, &layer.origin, obj);
            self.refuse_a_worktrees_dir_not_allowed(&layer.origin, layer.tier, obj);
            if is_own_project {
                for (name, server) in obj
                    .get("mcp_servers")
                    .and_then(|v| v.as_object())
                    .into_iter()
                    .flatten()
                {
                    let entry = self
                        .project_layer
                        .mcp_servers
                        .entry(name.clone())
                        .or_insert(serde_json::Value::Null);
                    merge_json_values(entry, server.clone());
                }
                let gated = trust::gate_layer(obj, &self.merged_json, self.withhold, self.sections);
                if self.withhold && !gated.is_empty() {
                    tracing::warn!(origin = %layer.origin, withheld = ?gated, "project is not trusted; settings only a trusted project may set are ignored");
                }
                self.project_layer
                    .requires_trust
                    .extend(gated.into_iter().map(|key| trust::Withheld {
                        key,
                        origin: layer.origin.clone(),
                    }));
            }
            if let Some(block) = obj
                .get_mut(permissions::KEY)
                .and_then(serde_json::Value::as_object_mut)
            {
                self.refuse_a_program_only_mode(&layer.origin, layer.tier, block);
                for (list, rules) in take_rule_lists(block) {
                    self.take_permission_rules(
                        &layer.origin,
                        layer.tier,
                        layer.machine_local,
                        list,
                        rules,
                    );
                }
            }
        }
        self.note_mcp_servers(layer.tier, &layer_json);
        for (key, error) in merge_fitting(&mut self.merged_json, layer_json, self.sections) {
            let error = crate::renamed::explain(&key, error);
            tracing::warn!(layer = layer.tier.name(), origin = %layer.origin, key, error, "settings value does not fit its setting, ignoring it");
            self.ignored.push(IgnoredValue {
                tier: layer.tier.name(),
                origin: layer.origin.clone(),
                key,
                error,
            });
        }
    }

    fn refuse_a_worktrees_dir_not_allowed(
        &mut self,
        origin: &str,
        tier: Tier,
        obj: &mut serde_json::Map<String, serde_json::Value>,
    ) {
        let Some(name) = obj.get("worktree_dir").and_then(|v| v.as_str()) else {
            return;
        };
        let Err(e) = self.layout.clone().with_worktrees_dir(name) else {
            return;
        };
        obj.remove("worktree_dir");
        let error = e.to_string();
        tracing::warn!(layer = tier.name(), origin = %origin, error, "settings value does not fit its setting, ignoring it");
        self.ignored.push(IgnoredValue {
            tier: tier.name(),
            origin: origin.to_string(),
            key: "worktree_dir".into(),
            error,
        });
    }

    fn refuse_a_program_only_mode(
        &mut self,
        origin: &str,
        tier: Tier,
        block: &mut serde_json::Map<String, serde_json::Value>,
    ) {
        let Some(mode) = block
            .get("mode")
            .and_then(|v| serde_json::from_value::<PermissionMode>(v.clone()).ok())
            .filter(|mode| !mode.is_user_settable())
        else {
            return;
        };
        block.remove("mode");
        let error = format!(
            "permission mode {mode:?} is program-only and cannot be set in a settings file"
        );
        tracing::warn!(layer = tier.name(), origin = %origin, error, "settings value does not fit its setting, ignoring it");
        self.ignored.push(IgnoredValue {
            tier: tier.name(),
            origin: origin.to_string(),
            key: format!("{}.mode", permissions::KEY),
            error,
        });
    }

    fn take_permission_rules(
        &mut self,
        origin: &str,
        tier: Tier,
        machine_local: bool,
        list: &str,
        rules: serde_json::Value,
    ) {
        let (rules, unread) = read_rule_list(list, rules);
        for (key, error) in unread {
            tracing::warn!(layer = tier.name(), origin = %origin, key, error, "permission rule does not read, ignoring it");
            self.ignored.push(IgnoredValue {
                tier: tier.name(),
                origin: origin.to_string(),
                key,
                error,
            });
        }
        for rule in rules {
            if self.tiers.is_own_project(tier) && rule.action == RuleBehavior::Allow {
                self.project_layer.requires_trust.push(trust::Withheld {
                    key: format!("{}.{list}({})", permissions::KEY, rule.tool),
                    origin: origin.to_string(),
                });
                if self.withhold {
                    continue;
                }
            }
            self.loaded_rules
                .push(LoadedRule::new(rule, tier, machine_local));
        }
    }

    fn note_mcp_servers(&mut self, tier: Tier, layer_json: &serde_json::Value) {
        let declared_in = if self.tiers.is_global(tier) {
            self.global_dir
        } else if self.tiers.is_own_project(tier) {
            self.local_dir.parent().unwrap_or(self.local_dir)
        } else {
            self.scene_dir
        };
        for (name, server) in layer_json
            .get("mcp_servers")
            .and_then(|v| v.as_object())
            .into_iter()
            .flatten()
        {
            if server.get("command").is_some() || !self.mcp_server_dirs.contains_key(name) {
                self.mcp_server_dirs
                    .insert(name.clone(), declared_in.to_path_buf());
            }
            // A project's own servers read nothing from this process's
            // environment (`confined_to_own_env`), so the names they
            // write are not secrets of the daemon's.
            if !self.tiers.is_own_project(tier) {
                self.mcp_secret_vars.extend(
                    base::interface::exec::env::variables_an_mcp_server_reads(server),
                );
            }
        }
    }
}

/// `obj`, a layer of `tier`, without the keys that tier is not read for:
/// [`crate::GLOBAL_ONLY_KEYS`] unless it is the global tier,
/// [`crate::NOT_FROM_PROJECT_KEYS`] in a project's own.
fn drop_keys_not_read_from(
    tier: Tier,
    is_global: bool,
    origin: &str,
    obj: &mut serde_json::Map<String, serde_json::Value>,
) {
    for key in crate::GLOBAL_ONLY_KEYS {
        if !is_global && obj.remove(*key).is_some() {
            tracing::warn!(
                layer = tier.name(),
                origin,
                key,
                "this key is read from the global tier only, ignoring it here"
            );
        }
    }
    for key in crate::NOT_FROM_PROJECT_KEYS {
        if tier == Tier::Project && !is_global && obj.remove(*key).is_some() {
            tracing::warn!(
                layer = tier.name(),
                origin,
                key,
                "a project's own settings cannot set this key, ignoring it"
            );
        }
    }
}

/// Before any session these settings start runs a child process: a provider
/// key is not read until a client is built, an MCP secret not until its server
/// connects, and a hook or `Bash` can run before that.
fn guard_loaded_values(values: &mut Settings, mcp_secret_vars: &[String]) {
    for name in values
        .providers
        .values()
        .filter_map(|p| p.api_key_env.as_deref())
        .chain(mcp_secret_vars.iter().map(String::as_str))
    {
        base::interface::exec::env::hold_back_credential(name);
    }
    if let Err(reason) = values.validate() {
        tracing::warn!("Settings validation: {reason}");
        values.permissions.mode = PermissionMode::default();
    }
}
