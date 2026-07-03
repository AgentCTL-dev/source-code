// SPDX-License-Identifier: Apache-2.0
//! Validate the typed client against the GOLDEN capability fixtures in
//! `contract/fixtures/capabilities/` — two of which are real `--capabilities`
//! captures from the reference binary (agentd 1.0.0; it resolves via
//! `agent_version`). This is the behavioral
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
        assert_eq!(v, ContractVersion { major: 2, minor: 0 }, "{name}");
    }
}

#[test]
fn default_capture_has_surfaces_off() {
    // Real v2 capture (agentd --capabilities --mode once): a once-mode build with
    // the management/metrics/a2a surfaces off.
    let m = load("default.json");
    assert_eq!(m.version(), Some("2.1.0")); // resolves via agent_version
    assert_eq!(m.mode.as_deref(), Some("once"));
    // No serve target ⇒ the management + metrics LISTENERS are off.
    assert!(!m.surfaces.management.is_served());
    assert!(!m.surfaces.metrics.is_served());
    // Contract 2.0: `surfaces.a2a` advertises the compiled A2A capability
    // (methods the build can serve) independent of whether a listener is bound —
    // so a once-mode, no-serve build still advertises it. (v1 tied a2a to the
    // management listener; the pivot decoupled them.)
    assert!(m.surfaces.a2a.is_served());
    assert_eq!(m.surfaces.shard, None);
    assert_eq!(m.intelligence.healthy, Health::Unknown);
    // claim is served as an object even when the transport surfaces are off.
    assert_eq!(
        m.surfaces.claim.as_ref().and_then(ClaimSurface::styles),
        Some(["tool".to_string(), "resource".to_string()].as_slice())
    );
    // operator tools are read from the manifest, never assumed. Contract 2.0
    // spells them as the a2a.* admin JSON-RPC methods.
    assert_eq!(
        m.surfaces.operator_tools,
        [
            "a2a.Drain",
            "a2a.LameDuck",
            "a2a.Pause",
            "a2a.Resume",
            "a2a.Cancel"
        ]
    );
}

#[test]
fn full_features_capture_has_surfaces_on() {
    // Real v2 capture: a reactive, fully-featured build serving mTLS HTTPS with
    // every surface on. Management is now an `https://` address (no vsock).
    let m = load("full-features.json");
    assert_eq!(m.surfaces.management.addr(), Some("https://0.0.0.0:8443"));
    assert_eq!(m.surfaces.metrics.addr(), Some("0.0.0.0:9090"));
    assert_eq!(m.surfaces.shard.as_deref(), Some("0/3"));

    let a2a = m.surfaces.a2a.info().expect("a2a served");
    assert_eq!(a2a.version, "1.0");
    assert!(a2a.streaming);
    assert_eq!(a2a.methods.len(), 6);
    // Contract 2.0: the A2A methods are the bare PascalCase spec-§9 binding.
    assert!(a2a.methods.iter().any(|x| x == "SendMessage"));

    assert_eq!(m.identity.namespace.as_deref(), Some("agents"));
    // exec was REMOVED in contract 2.0 — no build advertises it.
    assert!(!m.exec_enabled);
    assert!(m.allow_trifecta);
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
    // The supported major is 2 (contract 2.0); a 3.x agent is the future one
    // this client does not yet understand.
    let mut v = load_value("default.json");
    v["contract_version"] = serde_json::json!("3.0");
    let m: Manifest = serde_json::from_value(v).unwrap();
    assert!(matches!(
        m.negotiate(),
        Err(NegotiationError::UnsupportedMajor {
            found: 3,
            supported: 2
        })
    ));

    // And the legacy contract 1.x is now equally unsupported (the pivot moved
    // the major forward — a v1 agent no longer negotiates).
    let mut v = load_value("default.json");
    v["contract_version"] = serde_json::json!("1.0");
    let m: Manifest = serde_json::from_value(v).unwrap();
    assert!(matches!(
        m.negotiate(),
        Err(NegotiationError::UnsupportedMajor {
            found: 1,
            supported: 2
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
