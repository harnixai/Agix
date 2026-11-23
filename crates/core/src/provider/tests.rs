use super::*;

fn provider(id: &str, models: &[&str], tiers: [Option<&str>; 3]) -> ProviderConfig {
    ProviderConfig {
        id: id.into(),
        models: models.iter().map(|m| (*m).into()).collect(),
        tiers: Tiers {
            strong: tiers[0].map(str::to_string),
            balanced: tiers[1].map(str::to_string),
            fast: tiers[2].map(str::to_string),
        },
        ..Default::default()
    }
}

#[test]
fn missing_tiers_take_the_tier_above() {
    let p = provider("ds", &[], [Some("pro"), None, None]);
    assert_eq!(p.resolved_tiers().unwrap(), ResolvedTiers::all("pro"));
    let p = provider("ds", &[], [Some("pro"), Some("flash"), None]);
    let tiers = p.resolved_tiers().unwrap();
    assert_eq!(
        (tiers.balanced.as_str(), tiers.fast.as_str()),
        ("flash", "flash")
    );
}

#[test]
fn a_provider_without_strong_or_with_a_tier_outside_its_models_is_an_error() {
    let err = provider("ds", &[], [None, Some("flash"), None])
        .resolved_tiers()
        .unwrap_err();
    assert!(err.contains("tiers.strong"), "{err}");
    let err = provider("ds", &["pro"], [Some("pro"), Some("flash"), None])
        .resolved_tiers()
        .unwrap_err();
    assert!(
        err.contains("tiers.balanced") && err.contains("flash"),
        "{err}"
    );
}

#[test]
fn the_order_names_defined_providers_and_defaults_to_the_first() {
    let providers = vec![
        provider("ds", &[], [Some("pro"), None, None]),
        provider("claude", &[], [Some("opus"), None, None]),
    ];
    let order = routing_order(&providers, &[]).unwrap();
    assert_eq!(order.len(), 1);
    assert_eq!(order[0].0.id, "ds");
    let order = routing_order(&providers, &["claude".into(), "ds".into()]).unwrap();
    assert_eq!(order[0].0.id, "claude");
    let err = routing_order(&providers, &["ds".into(), "ghost".into()]).unwrap_err();
    assert!(err.contains("ghost"), "{err}");
}

#[test]
fn deepseek_spells_effort_its_own_way() {
    use crate::interface::model::Effort;
    assert_eq!(Vendor::Deepseek.effort(Effort::Medium), Effort::High);
    assert_eq!(Vendor::Deepseek.effort(Effort::Max), Effort::Max);
    assert_eq!(Vendor::Generic.effort(Effort::Medium), Effort::Medium);
    assert!(Vendor::Deepseek.passes_reasoning_back());
    assert_eq!(
        Vendor::Qwen.thinking_toggle(),
        Some(ThinkingToggle::EnableThinking)
    );
}
