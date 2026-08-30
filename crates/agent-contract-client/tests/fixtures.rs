//! Golden-fixture tests: the client parses REAL captures from the pinned
//! agentd binary (`contract/fixtures/capabilities/`). These are the ground
//! truth for the informational-manifest reader; the negotiation clocks are
//! covered by the vendored-schema tests in the crate itself.

use agent_contract_client::parse_manifest;

const FIXTURES: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../contract/fixtures/capabilities/"
);

fn load(name: &str) -> agent_contract_client::Manifest {
    let raw = std::fs::read_to_string(format!("{FIXTURES}{name}"))
        .unwrap_or_else(|e| panic!("read fixture {name}: {e}"));
    parse_manifest(&raw).unwrap_or_else(|e| panic!("parse fixture {name}: {e}"))
}

#[test]
fn every_capture_parses_and_identifies_runtime_1() {
    for name in ["agentd-1.3.1-default.json", "agentd-1.3.1-configured.json"] {
        let m = load(name);
        assert!(m.is_runtime_1(), "{name}: runtime generation");
        assert_eq!(m.version.as_deref(), Some("1.3.1"), "{name}: version");
    }
}

#[test]
fn default_capture_is_the_bare_daemonless_shape() {
    let m = load("agentd-1.3.1-default.json");
    assert!(m.a2a.is_none(), "no listener configured");
    assert!(!m.agent.instruction);
    assert_eq!(m.intelligence.endpoints, 0);
    assert!(m.internal_tools.iter().any(|t| t == "workflow.run"));
    assert!(!m.lifecycle.daemon);
}

#[test]
fn configured_capture_reports_listeners_workflows_and_admin_verbs() {
    let m = load("agentd-1.3.1-configured.json");
    let a2a = m.a2a.as_ref().expect("a2a block");
    assert_eq!(a2a.listen.as_deref(), Some("http://127.0.0.1:8420"));
    assert_eq!(
        m.admin_verbs(),
        [
            "a2a.drain",
            "a2a.lameduck",
            "a2a.cancel",
            "a2a.pause",
            "a2a.resume"
        ]
    );
    assert!(a2a.command_ops.iter().any(|c| c == "workflow.run"));

    // Start-kind inventory — including the KNOWN GAP the contract records:
    // the webhook workflow reports an EMPTY start_kinds list at 1.3.1 (U3).
    assert_eq!(m.workflow("hourly").unwrap().start_kinds, ["schedule"]);
    assert_eq!(m.workflow("watcher").unwrap().start_kinds, ["subscribe"]);
    assert_eq!(m.workflow("cmd").unwrap().start_kinds, ["a2a"]);
    assert!(
        m.workflow("hook").unwrap().start_kinds.is_empty(),
        "U3: webhook start unreported — if this starts failing, upstream fixed it; update the contract"
    );

    assert!(m.lifecycle.daemon);
    assert_eq!(m.lifecycle.run_until.as_deref(), Some("drained"));
}

#[test]
fn additive_unknown_keys_are_tolerated() {
    let raw = std::fs::read_to_string(format!("{FIXTURES}agentd-1.3.1-default.json")).unwrap();
    let mut v: serde_json::Value = serde_json::from_str(&raw).unwrap();
    v["some_future_block"] = serde_json::json!({ "with": ["structure"] });
    v["workflows"] = serde_json::json!([{ "name": "x", "novel_field": true }]);
    let m = parse_manifest(&v.to_string()).expect("additive tolerance");
    assert_eq!(m.workflows[0].name, "x");
}

#[test]
fn a_pre_rewrite_manifest_is_identified_as_unmanageable() {
    // The July-era shape: contract_version + surfaces, no runtime key.
    let old =
        r#"{"contract_version":"1.0","agent_version":"1.0.0","surfaces":{"management":false}}"#;
    let m = parse_manifest(old).expect("still parses (additive tolerance)");
    assert!(
        !m.is_runtime_1(),
        "no runtime key ⇒ pre-rewrite ⇒ refuse to manage"
    );
}
