// SPDX-License-Identifier: BUSL-1.1
//! Minimal, hand-rolled OCI artifact pull — just enough to resolve a
//! `workflows[].setRef` **WorkflowSet** bundle into its workflow documents.
//!
//! Digest-pinned ONLY: the reference MUST carry `@sha256:<hex>`, and that is
//! the integrity guarantee — the manifest bytes are checked against the pinned
//! digest, and every layer blob against its own digest, so a tampered or
//! swapped bundle is refused. A mutable tag is rejected (nothing to verify
//! against). Hand-rolled on `reqwest` + `sha2` to keep the workspace
//! deny-clean; every off-the-shelf OCI crate drags a heavy, license-noisy tail
//! (the same lesson as the artifacts S3 client).
//!
//! Bundle format (ORAS convention): an image manifest whose `layers[]` are the
//! workflow files, each blob's `org.opencontainers.image.title` annotation
//! giving its filename. Every layer is fetched, digest-checked, and returned.

use serde_json::Value;
use sha2::{Digest, Sha256};

/// One file extracted from a bundle.
pub struct BundleFile {
    pub name: String,
    pub content: Vec<u8>,
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::new().chain_update(bytes).finalize())
}

/// Split a digest-pinned reference into `(registry_authority, repository, digest)`.
/// Refuses a reference without an `@sha256:` digest.
fn parse_ref(reference: &str) -> Result<(String, String, String), String> {
    let reference = reference.strip_prefix("oci://").unwrap_or(reference);
    let (name, digest) = reference.split_once('@').ok_or_else(|| {
        format!("setRef {reference:?} is not digest-pinned — an OCI WorkflowSet ref must be `<registry>/<repo>@sha256:<hex>`")
    })?;
    if !digest.starts_with("sha256:") || digest.len() != "sha256:".len() + 64 {
        return Err(format!(
            "setRef digest {digest:?} is not a sha256:<64-hex> digest"
        ));
    }
    let (registry, repo) = name.split_once('/').ok_or_else(|| {
        format!("setRef {reference:?} has no registry authority (want `<registry>/<repo>@…`)")
    })?;
    if registry.is_empty() || repo.is_empty() {
        return Err(format!(
            "setRef {reference:?} has an empty registry or repository"
        ));
    }
    Ok((registry.to_string(), repo.to_string(), digest.to_string()))
}

/// `true` if this registry authority should be dialed over plaintext HTTP —
/// an in-cluster registry (`.svc`/`.svc.cluster.local`/single-label host) or one
/// listed in `AGENTCTL_OCI_INSECURE_REGISTRIES` (CSV). Everything else is HTTPS.
fn allow_http(registry: &str, insecure_csv: &str) -> bool {
    let host = registry.split(':').next().unwrap_or(registry);
    host == "localhost"
        || host.ends_with(".svc")
        || host.ends_with(".svc.cluster.local")
        || host.ends_with(".svc.cluster.local.")
        || !host.contains('.')
        || insecure_csv
            .split(',')
            .map(str::trim)
            .any(|h| !h.is_empty() && h == host)
}

/// Give an in-cluster registry authority an absolute (trailing-dot) FQDN so a
/// cluster wildcard search domain cannot capture the 4-dot Service name (the
/// `ndots:5` leak). A `host:port` keeps its port; external hosts are untouched.
fn absolutize(registry: &str) -> String {
    let (host, port) = match registry.rsplit_once(':') {
        Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) => (h, Some(p)),
        _ => (registry, None),
    };
    let host = if (host.ends_with(".svc") || host.ends_with(".svc.cluster.local"))
        && !host.ends_with('.')
    {
        format!("{host}.")
    } else {
        host.to_string()
    };
    match port {
        Some(p) => format!("{host}:{p}"),
        None => host,
    }
}

const MANIFEST_ACCEPT: &str =
    "application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.v2+json";

/// Pull and verify a WorkflowSet bundle. `insecure_csv` comes from
/// `AGENTCTL_OCI_INSECURE_REGISTRIES`.
pub async fn pull_bundle(
    http: &reqwest::Client,
    reference: &str,
    insecure_csv: &str,
) -> Result<Vec<BundleFile>, String> {
    let (registry, repo, digest) = parse_ref(reference)?;
    let scheme = if allow_http(&registry, insecure_csv) {
        "http"
    } else {
        "https"
    };
    let base = format!("{scheme}://{}/v2/{repo}", absolutize(&registry));

    // Manifest — verified against the pinned digest.
    let manifest_bytes = get(http, &format!("{base}/manifests/{digest}"), MANIFEST_ACCEPT).await?;
    if sha256_hex(&manifest_bytes) != digest["sha256:".len()..] {
        return Err(format!(
            "manifest digest mismatch for {reference:?} (tampered bundle or registry)"
        ));
    }
    let manifest: Value = serde_json::from_slice(&manifest_bytes)
        .map_err(|e| format!("parse manifest for {reference:?}: {e}"))?;

    let layers = manifest
        .get("layers")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("manifest for {reference:?} has no layers[]"))?;
    let mut files = Vec::new();
    for layer in layers {
        let ld = layer
            .get("digest")
            .and_then(Value::as_str)
            .ok_or("a layer has no digest")?;
        if !ld.starts_with("sha256:") {
            return Err(format!("layer digest {ld:?} is not sha256"));
        }
        let title = layer
            .pointer("/annotations/org.opencontainers.image.title")
            .and_then(Value::as_str)
            .unwrap_or(ld)
            .to_string();
        let blob = get(http, &format!("{base}/blobs/{ld}"), "*/*").await?;
        if sha256_hex(&blob) != ld["sha256:".len()..] {
            return Err(format!(
                "blob digest mismatch for layer {title:?} (tampered bundle)"
            ));
        }
        files.push(BundleFile {
            name: title,
            content: blob,
        });
    }
    Ok(files)
}

/// GET with a bounded timeout and the Docker/OCI `WWW-Authenticate` bearer
/// dance (anonymous pull works without it; GHCR/Docker Hub answer a 401 with a
/// token endpoint we satisfy and retry once).
async fn get(http: &reqwest::Client, url: &str, accept: &str) -> Result<Vec<u8>, String> {
    let once = |bearer: Option<String>| {
        let http = http.clone();
        let url = url.to_string();
        let accept = accept.to_string();
        async move {
            let mut req = http
                .get(&url)
                .header("accept", accept)
                .timeout(std::time::Duration::from_secs(30));
            if let Some(b) = bearer {
                req = req.header("authorization", format!("Bearer {b}"));
            }
            req.send().await.map_err(|e| format!("GET {url}: {e}"))
        }
    };
    let resp = once(None).await?;
    let resp = if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
        let challenge = resp
            .headers()
            .get("www-authenticate")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let token = fetch_token(http, &challenge).await?;
        once(Some(token)).await?
    } else {
        resp
    };
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("GET {url}: {status}: {}", body.trim()));
    }
    Ok(resp.bytes().await.map_err(|e| e.to_string())?.to_vec())
}

/// Parse a `Bearer realm="…",service="…",scope="…"` challenge and fetch an
/// anonymous pull token.
async fn fetch_token(http: &reqwest::Client, challenge: &str) -> Result<String, String> {
    let rest = challenge
        .trim()
        .strip_prefix("Bearer ")
        .ok_or_else(|| format!("unexpected auth challenge {challenge:?}"))?;
    let mut realm = None;
    let mut params: Vec<(String, String)> = Vec::new();
    for part in rest.split(',') {
        let Some((k, v)) = part.split_once('=') else {
            continue;
        };
        let (k, v) = (k.trim(), v.trim().trim_matches('"'));
        if k == "realm" {
            realm = Some(v.to_string());
        } else {
            params.push((k.to_string(), v.to_string()));
        }
    }
    let realm = realm.ok_or_else(|| format!("auth challenge {challenge:?} has no realm"))?;
    let resp = http
        .get(&realm)
        .query(&params)
        .timeout(std::time::Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| format!("token request: {e}"))?;
    let body: Value = resp
        .json()
        .await
        .map_err(|e| format!("token response: {e}"))?;
    body.get("token")
        .or_else(|| body.get("access_token"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| "token response had no token".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ref_requires_a_digest() {
        assert!(parse_ref("reg.example.com/wf/bundle:latest").is_err());
        assert!(parse_ref("reg.example.com/wf/bundle").is_err());
        let good = format!("reg.example.com:5000/wf/bundle@sha256:{}", "a".repeat(64));
        let (r, repo, d) = parse_ref(&good).unwrap();
        assert_eq!(r, "reg.example.com:5000");
        assert_eq!(repo, "wf/bundle");
        assert!(d.starts_with("sha256:"));
    }

    #[test]
    fn parse_ref_strips_oci_scheme() {
        let good = format!("oci://r/repo@sha256:{}", "b".repeat(64));
        assert_eq!(parse_ref(&good).unwrap().0, "r");
    }

    #[test]
    fn parse_ref_rejects_bad_digest() {
        assert!(parse_ref("r/repo@sha256:short").is_err());
        assert!(parse_ref("r/repo@md5:whatever").is_err());
    }

    #[test]
    fn in_cluster_registries_use_http() {
        assert!(allow_http("agentctl-oci.agentctl-system.svc:5000", ""));
        assert!(allow_http("localhost:5000", ""));
        assert!(allow_http("registry", "")); // single-label
        assert!(!allow_http("ghcr.io", ""));
        assert!(allow_http("ghcr.io", "ghcr.io")); // explicit insecure allowlist
    }

    #[test]
    fn absolutize_adds_a_trailing_dot_to_in_cluster_hosts() {
        assert_eq!(
            absolutize("agentctl-oci.agentctl-system.svc.cluster.local:5000"),
            "agentctl-oci.agentctl-system.svc.cluster.local.:5000"
        );
        assert_eq!(absolutize("reg.default.svc"), "reg.default.svc.");
        assert_eq!(absolutize("ghcr.io"), "ghcr.io"); // external untouched
        assert_eq!(absolutize("ghcr.io:443"), "ghcr.io:443");
        // already absolute — no double dot
        assert_eq!(
            absolutize("reg.default.svc.cluster.local."),
            "reg.default.svc.cluster.local."
        );
    }

    #[test]
    fn digest_of_empty_is_known() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }
}
