// SPDX-License-Identifier: Apache-2.0
//! Validate the typed client against the GOLDEN capability fixtures in
//! `contract/fixtures/capabilities/` — two of which are real `--capabilities`
//! captures from the reference binary (agent 2.5.0). This is the behavioral
//! ground-truth: if the client and the contract drift, these fail.

use agent_contract_client::*;

const FIXTURES: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../contract/fixtures/capabilities/"
);

fn load(name: &str) -> Manifest {
    let path = format!("{FIXTURES}{name}");
    let json = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    parse_manifest(&json).unwrap_or_else(|e| panic!("parse {name}: {e}"))
}

fn load_value(name: &str) -> serde_json::Value {
    let path = format!("{FIXTURES}{name}");
    serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap()
}

#[test]
fn every_fixture_parses_and_negotiates() {
    for name in [
        "default.json",
        "full-features.json",
        "reference-full.json",
        "minimal-degraded.json",
    ] {
        let m = load(name);
        let v = m
            .negotiate()
            .unwrap_or_else(|e| panic!("{name} negotiate: {e}"));
        assert_eq!(v, ContractVersion { major: 1, minor: 0 }, "{name}");
    }
}

#[test]
fn default_capture_has_surfaces_off() {
    // Real capture: a once-mode build with the management/metrics/a2a surfaces off.
    let m = load("default.json");
    assert_eq!(m.version(), Some("3.0.0")); // resolves via agent_version
    assert_eq!(m.mode.as_deref(), Some("once"));
    assert!(!m.surfaces.management.is_served());
    assert!(!m.surfaces.metrics.is_served());
    assert!(!m.surfaces.a2a.is_served());
    assert_eq!(m.surfaces.shard, None);
    assert_eq!(m.intelligence.healthy, Health::Unknown);
    // claim is served as an object even when the transport surfaces are off.
    assert_eq!(
        m.surfaces.claim.as_ref().and_then(ClaimSurface::styles),
        Some(["tool".to_string(), "resource".to_string()].as_slice())
    );
    // operator tools are read from the manifest, never assumed.
    assert_eq!(
        m.surfaces.operator_tools,
        ["drain", "lame-duck", "pause", "resume", "cancel"]
    );
}

#[test]
fn full_features_capture_has_surfaces_on() {
    // Real capture: a reactive, fully-featured build with every surface served.
    let m = load("full-features.json");
    assert_eq!(m.surfaces.management.addr(), Some("vsock:7000"));
    assert_eq!(m.surfaces.metrics.addr(), Some("127.0.0.1:9090"));
    assert_eq!(m.surfaces.shard.as_deref(), Some("0/3"));

    let a2a = m.surfaces.a2a.info().expect("a2a served");
    assert_eq!(a2a.version, "1.0");
    assert!(a2a.streaming);
    assert_eq!(a2a.methods.len(), 6);
    assert!(a2a.methods.iter().any(|x| x == "a2a.SendMessage"));

    assert_eq!(m.identity.namespace.as_deref(), Some("agents"));
    assert!(m.exec_enabled && m.allow_trifecta);
    assert_eq!(m.mcp_servers.first().map(|s| s.name.as_str()), Some("fs"));
}

#[test]
fn additive_tolerance_unknown_keys_ignored() {
    // A newer agent that adds an unknown surface key, an unknown top-level field,
    // and an unknown operator tool must still parse (additive-by-minor).
    let mut v = load_value("default.json");
    v["surfaces"]["future_surface"] = serde_json::json!("ignored");
    v["a_brand_new_top_level_field"] = serde_json::json!(42);
    let m: Manifest = serde_json::from_value(v).expect("additive fields tolerated");
    assert!(!m.surfaces.management.is_served());
}

#[test]
fn refuses_unknown_major_but_parses() {
    // An unknown MAJOR parses fine (still a manifest) but fails negotiation.
    let mut v = load_value("default.json");
    v["contract_version"] = serde_json::json!("2.0");
    let m: Manifest = serde_json::from_value(v).unwrap();
    assert!(matches!(
        m.negotiate(),
        Err(NegotiationError::UnsupportedMajor {
            found: 2,
            supported: 1
        })
    ));
}

#[test]
fn rejects_malformed_a2a_sum_type() {
    // surfaces.a2a is false|object — `true` is not a valid sum-type branch.
    let mut v = load_value("default.json");
    v["surfaces"]["a2a"] = serde_json::json!(true);
    assert!(serde_json::from_value::<Manifest>(v).is_err());
}
