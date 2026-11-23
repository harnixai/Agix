//! Settings keys owned by a module rather than by the settings crate: a
//! [`Section`] names its key, its type, and what a project that is not
//! trusted may do with it, and [`Sections`] is the list a build registers
//! once so that loading, trust gating, unknown-key reporting and the schema
//! all know the same keys.

use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;

use crate::scope::Scope;
use crate::tier_gate::ProjectKey;

pub trait Section: Serialize + DeserializeOwned + schemars::JsonSchema + Default + 'static {
    const KEY: &'static str;
    /// Relative to `KEY` (`""` is the key itself); the grammar of `tier_gate::PROJECT_TIER`.
    const PROJECT_TIER: &'static [(&'static str, ProjectKey)];
    /// Where the section is read — see `scope::SCOPES`.
    const SCOPE: Scope;
}

#[derive(Clone, Copy)]
pub struct SectionSpec {
    pub key: &'static str,
    pub project_tier: &'static [(&'static str, ProjectKey)],
    pub scope: Scope,
    check: fn(&Value) -> Result<(), String>,
    schema: fn(&mut schemars::SchemaGenerator) -> schemars::Schema,
    default: fn() -> Value,
}

impl SectionSpec {
    pub fn of<S: Section>() -> Self {
        Self {
            key: S::KEY,
            project_tier: S::PROJECT_TIER,
            scope: S::SCOPE,
            check: |value| {
                serde_json::from_value::<S>(value.clone())
                    .map(drop)
                    .map_err(|e| e.to_string())
            },
            schema: |generator| generator.subschema_for::<S>(),
            default: || serde_json::to_value(S::default()).unwrap_or(Value::Null),
        }
    }

    /// A section this build does not read (its module is compiled out): the
    /// key is known and takes any value, and neither a project that is not
    /// trusted nor a session can set it.
    pub fn inert(key: &'static str) -> Self {
        Self {
            key,
            project_tier: &[],
            scope: Scope::Pair,
            check: |_| Ok(()),
            schema: |_| schemars::Schema::from(true),
            default: || Value::Null,
        }
    }

    pub(crate) fn check(&self, value: &Value) -> Result<(), String> {
        (self.check)(value)
    }

    pub(crate) fn schema(&self, generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        (self.schema)(generator)
    }

    pub(crate) fn default_value(&self) -> Value {
        (self.default)()
    }
}

#[derive(Clone, Default)]
pub struct Sections(Vec<SectionSpec>);

impl Sections {
    pub fn none() -> Self {
        Self::default()
    }

    /// Panics on a key that is already registered or that a core field owns.
    pub fn with<S: Section>(self) -> Self {
        self.with_spec(SectionSpec::of::<S>())
    }

    pub fn with_spec(mut self, spec: SectionSpec) -> Self {
        assert!(
            self.get(spec.key).is_none()
                && !crate::schema::known_core_keys()
                    .iter()
                    .any(|key| key == spec.key),
            "settings key `{}` is registered twice or belongs to a core setting",
            spec.key
        );
        self.0.push(spec);
        self
    }

    pub fn get(&self, key: &str) -> Option<&SectionSpec> {
        self.0.iter().find(|spec| spec.key == key)
    }

    pub fn iter(&self) -> impl Iterator<Item = &SectionSpec> {
        self.0.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Default, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
    #[serde(default)]
    struct Probe {
        flag: bool,
    }

    impl Section for Probe {
        const KEY: &'static str = "probe";
        const PROJECT_TIER: &'static [(&'static str, ProjectKey)] = &[("flag", ProjectKey::Free)];
        const SCOPE: Scope = Scope::Session;
    }

    #[test]
    #[should_panic(expected = "registered twice")]
    fn a_key_registers_once() {
        let _ = Sections::none().with::<Probe>().with::<Probe>();
    }

    #[test]
    #[should_panic(expected = "core setting")]
    fn a_core_key_cannot_be_taken() {
        let _ = Sections::none().with_spec(SectionSpec::inert("prompt"));
    }

    #[test]
    fn a_registered_section_checks_its_value() {
        let spec = SectionSpec::of::<Probe>();
        assert!(spec.check(&serde_json::json!({"flag": true})).is_ok());
        assert!(spec.check(&serde_json::json!({"flag": "yes"})).is_err());
    }
}
