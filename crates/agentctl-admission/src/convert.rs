// SPDX-License-Identifier: BUSL-1.1
//! # CRD conversion webhook (P2-1b)
//!
//! `POST /convert` — the `ConversionReview` handler the multi-version
//! `Agent`/`AgentFleet` CRDs point at. Spec conversion is the pure
//! `agent_api::v1alpha2::convert` mapping (v1→v2 lossless; v2→v1 lossy —
//! ConversionReview carries no warnings channel, so the deprecation/drop
//! warnings surface at ADMISSION time instead, where AdmissionReview supports
//! them). Status: v2's status is a superset of v1's, so both directions map
//! field-for-field (v2-only `renderedHash`/`bundles` drop downward — old
//! readers have no use for them).
//!
//! A single unconvertible object fails the WHOLE review (the apiserver
//! demands all-or-nothing), with the object's index in the message.

use serde_json::{json, Map, Value};

/// Handle a ConversionReview: convert every object to `desiredAPIVersion`.
pub fn convert_review(review: &Value) -> Value {
    let request = &review["request"];
    let uid = request["uid"].as_str().unwrap_or_default();
    let desired = request["desiredAPIVersion"].as_str().unwrap_or_default();
    let empty = Vec::new();
    let objects = request["objects"].as_array().unwrap_or(&empty);

    let mut converted = Vec::with_capacity(objects.len());
    for (i, obj) in objects.iter().enumerate() {
        match convert_object(obj, desired) {
            Ok(o) => converted.push(o),
            Err(e) => {
                return json!({
                    "apiVersion": "apiextensions.k8s.io/v1",
                    "kind": "ConversionReview",
                    "response": {
                        "uid": uid,
                        "result": { "status": "Failed", "message": format!("object {i}: {e}") },
                    }
                });
            }
        }
    }
    json!({
        "apiVersion": "apiextensions.k8s.io/v1",
        "kind": "ConversionReview",
        "response": {
            "uid": uid,
            "result": { "status": "Success" },
            "convertedObjects": converted,
        }
    })
}

/// Convert one object to the desired apiVersion (identity when it already is).
pub fn convert_object(obj: &Value, desired: &str) -> Result<Value, String> {
    let api_version = obj["apiVersion"].as_str().unwrap_or_default();
    let kind = obj["kind"].as_str().unwrap_or_default();
    if api_version == desired {
        return Ok(obj.clone());
    }
    let (from, to) = (
        api_version.rsplit('/').next().unwrap_or_default(),
        desired.rsplit('/').next().unwrap_or_default(),
    );

    let spec = obj.get("spec").cloned().unwrap_or(Value::Null);
    let converted_spec = match (kind, from, to) {
        ("Agent", "v1alpha1", "v1alpha2") => {
            let v1: agent_api::AgentSpec = serde_json::from_value(spec)
                .map_err(|e| format!("parse v1alpha1 Agent spec: {e}"))?;
            let (v2, _warnings) = agent_api::v1alpha2::convert::agent_v1_to_v2(&v1);
            serde_json::to_value(v2).map_err(|e| e.to_string())?
        }
        ("Agent", "v1alpha2", "v1alpha1") => {
            let v2: agent_api::v1alpha2::AgentSpec = serde_json::from_value(spec)
                .map_err(|e| format!("parse v1alpha2 Agent spec: {e}"))?;
            let (v1, _warnings) = agent_api::v1alpha2::convert::agent_v2_to_v1(&v2);
            serde_json::to_value(v1).map_err(|e| e.to_string())?
        }
        ("AgentFleet", "v1alpha1", "v1alpha2") => {
            let v1: agent_api::AgentFleetSpec = serde_json::from_value(spec)
                .map_err(|e| format!("parse v1alpha1 AgentFleet spec: {e}"))?;
            let (v2, _warnings) = agent_api::v1alpha2::convert::fleet_v1_to_v2(&v1);
            serde_json::to_value(v2).map_err(|e| e.to_string())?
        }
        ("AgentFleet", "v1alpha2", "v1alpha1") => {
            let v2: agent_api::v1alpha2::AgentFleetSpec = serde_json::from_value(spec)
                .map_err(|e| format!("parse v1alpha2 AgentFleet spec: {e}"))?;
            let (v1, _warnings) = agent_api::v1alpha2::convert::fleet_v2_to_v1(&v2);
            serde_json::to_value(v1).map_err(|e| e.to_string())?
        }
        _ => {
            return Err(format!(
                "no conversion for kind {kind:?} {from} → {to} (only Agent/AgentFleet between v1alpha1 and v1alpha2)"
            ));
        }
    };

    let mut out = Map::new();
    out.insert("apiVersion".into(), json!(desired));
    out.insert("kind".into(), json!(kind));
    out.insert(
        "metadata".into(),
        obj.get("metadata").cloned().unwrap_or(Value::Null),
    );
    out.insert("spec".into(), converted_spec);
    if let Some(status) = obj.get("status") {
        // v2 status ⊇ v1 status field-for-field; downward, the v2-only keys
        // (renderedHash/bundles) are pruned by the v1 schema anyway, but
        // strip them here so the emitted object is exactly the target shape.
        let mut status = status.clone();
        if kind == "Agent" && to == "v1alpha1" {
            if let Some(m) = status.as_object_mut() {
                m.remove("renderedHash");
                m.remove("bundles");
            }
        }
        out.insert("status".into(), status);
    }
    Ok(Value::Object(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v1_agent() -> Value {
        json!({
            "apiVersion": "agentctl.dev/v1alpha1",
            "kind": "Agent",
            "metadata": { "name": "triage", "namespace": "org-acme", "labels": { "team": "eng" } },
            "spec": {
                "mode": "loop",
                "image": "agentd:1.3.1",
                "instruction": "work",
                "loop": { "interval": "30s" },
                "handle": "triage",
            },
            "status": {
                "phase": "Running",
                "conditions": [{ "type": "Ready", "status": "True" }],
                "contract": { "contractVersion": "1.0" },
            },
        })
    }

    #[test]
    fn v1_agent_up_converts_and_round_trips() {
        let up = convert_object(&v1_agent(), "agentctl.dev/v1alpha2").unwrap();
        assert_eq!(up["apiVersion"], "agentctl.dev/v1alpha2");
        assert_eq!(up["spec"]["shape"], "daemon");
        assert_eq!(up["spec"]["triggers"][0]["loop"]["interval"], "30s");
        // metadata + status carry through untouched.
        assert_eq!(up["metadata"]["labels"]["team"], "eng");
        assert_eq!(up["status"]["contract"]["contractVersion"], "1.0");

        let down = convert_object(&up, "agentctl.dev/v1alpha1").unwrap();
        assert_eq!(down["spec"]["mode"], "loop");
        assert_eq!(down["spec"]["loop"]["interval"], "30s");
        assert_eq!(down["spec"]["image"], "agentd:1.3.1");
        assert_eq!(down["status"]["phase"], "Running");
    }

    #[test]
    fn review_converts_all_or_fails_naming_the_object() {
        let review = json!({
            "request": {
                "uid": "u1",
                "desiredAPIVersion": "agentctl.dev/v1alpha2",
                "objects": [v1_agent(), v1_agent()],
            }
        });
        let resp = convert_review(&review);
        assert_eq!(resp["response"]["result"]["status"], "Success");
        assert_eq!(
            resp["response"]["convertedObjects"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(resp["response"]["uid"], "u1");

        let bad = json!({
            "request": {
                "uid": "u2",
                "desiredAPIVersion": "agentctl.dev/v1alpha2",
                "objects": [json!({ "apiVersion": "agentctl.dev/v1alpha1", "kind": "Widget" })],
            }
        });
        let resp = convert_review(&bad);
        assert_eq!(resp["response"]["result"]["status"], "Failed");
        assert!(resp["response"]["result"]["message"]
            .as_str()
            .unwrap()
            .contains("object 0"));
    }

    #[test]
    fn same_version_is_identity() {
        let obj = v1_agent();
        let out = convert_object(&obj, "agentctl.dev/v1alpha1").unwrap();
        assert_eq!(out, obj);
    }
}
