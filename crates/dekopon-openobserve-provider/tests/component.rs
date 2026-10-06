//! Real component ABI and manifest conformance. No live store is contacted.
use dekopon_openobserve_provider::OpenObserveProvider;
use dekopon_provider_sdk::provider;
use dekopon_provider_sdk_testkit::conformance;

#[test]
fn real_component_conforms_to_typed_sdk_manifest_and_imports() {
    let Some(component) = std::env::var_os("DEKOPON_PROVIDER_COMPONENT") else {
        return;
    };
    conformance::<OpenObserveProvider>(component).expect("typed imports, manifest, help, no WASI");
}

#[test]
fn manifest_fixture_matches_the_typed_manifest() {
    let generated = format!(
        "{}\n",
        serde_json::to_string_pretty(&provider::manifest::<OpenObserveProvider>().unwrap())
            .unwrap()
    );
    assert_eq!(generated, include_str!("fixtures/manifest.json"));
}

#[test]
fn three_words_and_four_closed_low_risk_reads() {
    let manifest = provider::manifest::<OpenObserveProvider>().expect("typed manifest");
    assert_eq!(manifest.command_words, ["openobserve", "agent", "broker"]);
    assert_eq!(manifest.capabilities.len(), 4);
    for capability in manifest.capabilities {
        assert_eq!(
            capability.effect,
            dekopon_provider_sdk::EffectKind::ReadOnly
        );
        assert_eq!(capability.risk, dekopon_provider_sdk::RiskLevel::Low);
        assert_eq!(capability.input_schema["additionalProperties"], false);
        for forbidden in ["url", "org", "stream", "sql"] {
            assert!(
                capability.input_schema["properties"]
                    .get(forbidden)
                    .is_none()
            );
        }
        if capability.id.as_str() == "openobserve.trace"
            || capability.id.as_str() == "openobserve.broker-providers"
        {
            assert_eq!(
                capability.input_schema["properties"]["sinceSeconds"]["maximum"],
                86400
            );
        } else {
            assert!(
                capability.input_schema["properties"]
                    .get("sinceSeconds")
                    .is_none()
            );
        }
    }
}
