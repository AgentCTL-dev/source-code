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

/// The v2-spec stash annotation: written onto the v1 REPRESENTATION at
/// down-conversion so a v1-mediated write (the operator's finalizer/status
/// patches, an old client's update) cannot erase v2-only fields. Storage is
/// v1alpha2: every v1 write round-trips v2→v1→v2 through this webhook, and
/// without the stash that round trip silently dropped `class`/`services`/…
/// (observed live: the operator's status patch erased a stored grant).
pub const V2_STASH_ANNOTATION: &str = "agentctl.dev/v1alpha2-spec";

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
    let mut metadata = obj.get("metadata").cloned().unwrap_or(Value::Null);
    let stashed: Option<Value> = metadata
        .get("annotations")
        .and_then(|a| a.get(V2_STASH_ANNOTATION))
        .and_then(Value::as_str)
        .and_then(|raw| serde_json::from_str(raw).ok());

    let converted_spec = match (kind, from, to) {
        ("Agent", "v1alpha1", "v1alpha2") => {
            let v1: agent_api::AgentSpec = serde_json::from_value(spec)
                .map_err(|e| format!("parse v1alpha1 Agent spec: {e}"))?;
            let (fresh, _warnings) = agent_api::v1alpha2::convert::agent_v1_to_v2(&v1);
            strip_stash(&mut metadata);
            match stashed {
                // The stash restores what the v1 view cannot express. When
                // the v1 writer did NOT touch the v1-visible surface (the
                // stashed spec's own down-conversion equals the incoming v1),
                // the stashed v2 spec is returned VERBATIM; otherwise the
                // fresh conversion wins for the v1-visible fields and the
                // stashed v2-only fields are merged back on top.
                Some(stash) => {
                    match serde_json::from_value::<agent_api::v1alpha2::AgentSpec>(stash.clone()) {
                        Ok(stashed_v2) => {
                            let (down, _) =
                                agent_api::v1alpha2::convert::agent_v2_to_v1(&stashed_v2);
                            if serde_json::to_value(&down).ok() == serde_json::to_value(&v1).ok() {
                                stash
                            } else {
                                merge_v2_only(
                                    serde_json::to_value(fresh).map_err(|e| e.to_string())?,
                                    &stash,
                                )
                            }
                        }
                        Err(_) => serde_json::to_value(fresh).map_err(|e| e.to_string())?,
                    }
                }
                None => serde_json::to_value(fresh).map_err(|e| e.to_string())?,
            }
        }
        ("Agent", "v1alpha2", "v1alpha1") => {
            let v2: agent_api::v1alpha2::AgentSpec = serde_json::from_value(spec.clone())
                .map_err(|e| format!("parse v1alpha2 Agent spec: {e}"))?;
            let (v1, _warnings) = agent_api::v1alpha2::convert::agent_v2_to_v1(&v2);
            set_stash(&mut metadata, &spec)?;
            serde_json::to_value(v1).map_err(|e| e.to_string())?
        }
        ("AgentFleet", "v1alpha1", "v1alpha2") => {
            let v1: agent_api::AgentFleetSpec = serde_json::from_value(spec)
                .map_err(|e| format!("parse v1alpha1 AgentFleet spec: {e}"))?;
            let (fresh, _warnings) = agent_api::v1alpha2::convert::fleet_v1_to_v2(&v1);
            strip_stash(&mut metadata);
            match stashed {
                Some(stash) => {
                    match serde_json::from_value::<agent_api::v1alpha2::AgentFleetSpec>(
                        stash.clone(),
                    ) {
                        Ok(stashed_v2) => {
                            let (down, _) =
                                agent_api::v1alpha2::convert::fleet_v2_to_v1(&stashed_v2);
                            if serde_json::to_value(&down).ok() == serde_json::to_value(&v1).ok() {
                                stash
                            } else {
                                // Same rule as the Agent arm: the v1 writer's
                                // surface wins, the stashed v2-only fields
                                // (spec.partitioning, template extras) merge
                                // back on top rather than being erased.
                                merge_v2_only(
                                    serde_json::to_value(fresh).map_err(|e| e.to_string())?,
                                    &stash,
                                )
                            }
                        }
                        Err(_) => serde_json::to_value(fresh).map_err(|e| e.to_string())?,
                    }
                }
                None => serde_json::to_value(fresh).map_err(|e| e.to_string())?,
            }
        }
        ("AgentFleet", "v1alpha2", "v1alpha1") => {
            let v2: agent_api::v1alpha2::AgentFleetSpec = serde_json::from_value(spec.clone())
                .map_err(|e| format!("parse v1alpha2 AgentFleet spec: {e}"))?;
            let (v1, _warnings) = agent_api::v1alpha2::convert::fleet_v2_to_v1(&v2);
            set_stash(&mut metadata, &spec)?;
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
    out.insert("metadata".into(), metadata);
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

/// Write the stash annotation onto a (v1-bound) metadata value.
fn set_stash(metadata: &mut Value, v2_spec: &Value) -> Result<(), String> {
    let raw = serde_json::to_string(v2_spec).map_err(|e| e.to_string())?;
    if metadata.is_null() {
        *metadata = json!({});
    }
    let m = metadata
        .as_object_mut()
        .ok_or("metadata is not an object")?;
    let ann = m.entry("annotations").or_insert_with(|| json!({}));
    ann.as_object_mut()
        .ok_or("annotations is not an object")?
        .insert(V2_STASH_ANNOTATION.into(), json!(raw));
    Ok(())
}

/// Drop the stash from a (v2-bound) metadata value — it exists only on the
/// v1 representation.
fn strip_stash(metadata: &mut Value) {
    if let Some(ann) = metadata
        .get_mut("annotations")
        .and_then(Value::as_object_mut)
    {
        ann.remove(V2_STASH_ANNOTATION);
    }
}

/// Overlay the v2-only fields from `stash` onto a fresh v1-derived v2 spec
/// (the v1 writer changed the v1-visible surface; the fields v1 cannot see
/// still survive).
fn merge_v2_only(mut fresh: Value, stash: &Value) -> Value {
    const V2_ONLY: &[&str] = &[
        "class",
        "services",
        "skills",
        "store",
        "peers",
        "approval",
        "priority",
        "lifecycle",
        "expose",
        // Fleet-level v2-only surface (RFC 0034).
        "partitioning",
    ];
    if let (Some(f), Some(s)) = (fresh.as_object_mut(), stash.as_object()) {
        for key in V2_ONLY {
            if let Some(v) = s.get(*key) {
                // `expose` exists in both worlds (v1 surfaces map onto it):
                // the fresh (v1-derived) value wins there; pure v2-only keys
                // restore from the stash.
                if *key == "expose" {
                    f.entry(key.to_string()).or_insert_with(|| v.clone());
                } else {
                    f.insert(key.to_string(), v.clone());
                }
            }
        }
    }
    fresh
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

    /// The live defect this stash fixes: storage is v2, the operator writes
    /// through v1 — v2→v1→v2 must NOT erase v2-only fields.
    #[test]
    fn v1_mediated_write_preserves_v2_only_fields() {
        let v2 = json!({
            "apiVersion": "agentctl.dev/v1alpha2",
            "kind": "Agent",
            "metadata": { "name": "ok", "namespace": "d" },
            "spec": {
                "shape": "daemon",
                "class": "guarded",
                "runtime": { "image": "agentd:1.3.1" },
                "expose": { "a2a": true },
                "services": [{ "name": "tickets", "allow": ["ticket_read"] }],
                "capabilities": { "egress": true },
            },
        });
        // Down to v1 (what a v1 reader/writer sees) — the stash rides along.
        let down = convert_object(&v2, "agentctl.dev/v1alpha1").unwrap();
        assert!(down["metadata"]["annotations"][V2_STASH_ANNOTATION].is_string());
        assert!(
            down["spec"].get("services").is_none(),
            "v1 view drops v2-only"
        );

        // The v1 writer touches nothing v1-visible (a status/finalizer patch)
        // → back up to v2: the stashed spec returns VERBATIM.
        let up = convert_object(&down, "agentctl.dev/v1alpha2").unwrap();
        assert_eq!(up["spec"]["services"][0]["name"], "tickets");
        assert_eq!(up["spec"]["class"], "guarded");
        assert!(
            up["metadata"]["annotations"]
                .get(V2_STASH_ANNOTATION)
                .is_none(),
            "the stash lives only on the v1 representation"
        );

        // The v1 writer CHANGES the v1-visible surface (new image): the fresh
        // fields win, the v2-only fields still survive.
        let mut edited = down.clone();
        edited["spec"]["image"] = json!("agentd:9.9.9");
        let up = convert_object(&edited, "agentctl.dev/v1alpha2").unwrap();
        assert_eq!(up["spec"]["runtime"]["image"], "agentd:9.9.9");
        assert_eq!(up["spec"]["services"][0]["name"], "tickets");
        assert_eq!(up["spec"]["class"], "guarded");
    }

    #[test]
    fn same_version_is_identity() {
        let obj = v1_agent();
        let out = convert_object(&obj, "agentctl.dev/v1alpha1").unwrap();
        assert_eq!(out, obj);
    }
}
