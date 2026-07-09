// SPDX-License-Identifier: Apache-2.0
//! A minimal **AAuth-verifying remote MCP server** — the e2e stand-in for a
//! third-party resource that authorizes agents by identity (RFC 0024 phase 0).
//!
//! It does what `guide-mcp-server-auth` rung 1 prescribes, for real:
//!  1. parse the three RFC 9421 headers (`Signature-Input` / `Signature` /
//!     `Signature-Key`, labels correlated),
//!  2. verify the presented **agent token** (`aa-agent+jwt`) against the Agent
//!     Provider's published JWKS (`{iss}/.well-known/aauth-agent.json` →
//!     `jwks_uri` → key by `kid`),
//!  3. reconstruct the signature base (`@method` `@authority` `@path`
//!     `signature-key` + `@signature-params`) and verify the HTTP signature
//!     with the token's `cnf.jwk` (proof of possession),
//!  4. serve just enough MCP (initialize / tools/list) for a conformant agent
//!     to consider the server connected — every accepted call was SIGNED.
//!
//! Unsigned calls get the spec challenge (`401` +
//! `AAuth-Requirement: requirement=agent-token`); bad signatures get `401` +
//! `Signature-Error`. `GET /stats` exposes the counters the e2e asserts.
//!
//! Serves HTTPS with a mounted cert (cluster CA via cert-manager) so the CEL
//! `aauth ⇒ https endpoint` rule holds and the dialing agent's `--tls-ca`
//! trust applies. A fixture, not a product component.

use std::io::{Read as _, Write as _};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

#[derive(Default)]
struct Stats {
    signed_ok: AtomicU64,
    unsigned_rejected: AtomicU64,
    bad_sig_rejected: AtomicU64,
}

#[derive(Clone)]
struct App {
    stats: Arc<Stats>,
    /// `host:port` of the Agent Provider whose JWKS anchors trust (plaintext
    /// in-cluster for the e2e). From `APD_HOST` (default `apd.default.svc.cluster.local:80`).
    apd_host: String,
}

#[tokio::main]
async fn main() {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("install ring crypto provider");

    let apd_host = std::env::var("APD_HOST")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "apd.default.svc.cluster.local:80".to_string());
    let tls_dir = std::env::var("TLS_DIR").unwrap_or_else(|_| "/etc/mock/tls".to_string());

    let app = App {
        stats: Arc::new(Stats::default()),
        apd_host,
    };
    let router = Router::new()
        .route("/mcp", post(mcp))
        .route("/stats", get(stats))
        .with_state(app);

    let tls = axum_server::tls_rustls::RustlsConfig::from_pem_file(
        format!("{tls_dir}/tls.crt"),
        format!("{tls_dir}/tls.key"),
    )
    .await
    .expect("load serving cert");
    let addr: std::net::SocketAddr = "0.0.0.0:8443".parse().unwrap();
    eprintln!("mock-aauth-mcp: serving https on {addr}");
    axum_server::bind_rustls(addr, tls)
        .serve(router.into_make_service())
        .await
        .expect("serve");
}

async fn stats(State(app): State<App>) -> Json<Value> {
    Json(json!({
        "signed_ok": app.stats.signed_ok.load(Ordering::Relaxed),
        "unsigned_rejected": app.stats.unsigned_rejected.load(Ordering::Relaxed),
        "bad_sig_rejected": app.stats.bad_sig_rejected.load(Ordering::Relaxed),
    }))
}

async fn mcp(State(app): State<App>, headers: HeaderMap, body: String) -> Response {
    // 1. The three signature headers must all be present, else challenge.
    let (Some(sig_input), Some(sig), Some(sig_key)) = (
        header(&headers, "signature-input"),
        header(&headers, "signature"),
        header(&headers, "signature-key"),
    ) else {
        app.stats.unsigned_rejected.fetch_add(1, Ordering::Relaxed);
        return (
            StatusCode::UNAUTHORIZED,
            [("aauth-requirement", "requirement=agent-token")],
            "agent token required",
        )
            .into_response();
    };

    let resolve = |kid: &str| app.jwks_key(kid);
    match verify(&headers, &sig_input, &sig, &sig_key, resolve) {
        Ok(agent) => {
            app.stats.signed_ok.fetch_add(1, Ordering::Relaxed);
            mcp_reply(&agent, &body)
        }
        Err(e) => {
            app.stats.bad_sig_rejected.fetch_add(1, Ordering::Relaxed);
            eprintln!("mock-aauth-mcp: reject: {e}");
            (
                StatusCode::UNAUTHORIZED,
                [("signature-error", "error=invalid_signature")],
                format!("invalid signature: {e}"),
            )
                .into_response()
        }
    }
}

/// Serve just enough MCP for a conformant agent's connect handshake. Every
/// request reaching here was verified.
fn mcp_reply(agent: &str, body: &str) -> Response {
    let req: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let id = req.get("id").cloned();
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    // A notification (no id) is acknowledged without a body.
    let Some(id) = id else {
        return StatusCode::ACCEPTED.into_response();
    };
    let result = match method {
        "initialize" => json!({
            "protocolVersion": "2025-06-18",
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "mock-aauth-mcp", "version": "0" },
        }),
        "tools/list" => json!({ "tools": [] }),
        // Anything else (incl. legacy discovery probes): method-not-found so
        // the client falls back to the standard handshake.
        _ => {
            let resp = json!({ "jsonrpc": "2.0", "id": id,
                "error": { "code": -32601, "message": format!("no {method} here") } });
            return (
                StatusCode::OK,
                [("mcp-session-id", "mock-session-1")],
                Json(resp),
            )
                .into_response();
        }
    };
    let resp = json!({ "jsonrpc": "2.0", "id": id, "result": result, "_verified_agent": agent });
    (
        StatusCode::OK,
        [("mcp-session-id", "mock-session-1")],
        Json(resp),
    )
        .into_response()
}

fn header(h: &HeaderMap, name: &str) -> Option<String> {
    h.get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

/// The full rung-1 verification. Returns the verified agent identifier.
/// `resolve_ap_key(kid)` yields the Agent Provider's Ed25519 public key bytes
/// for that `kid` (a live JWKS fetch in the server; a stub in tests) — so the
/// crypto path (JWT trust + RFC 9421 signature base + PoP) is unit-testable.
fn verify(
    headers: &HeaderMap,
    sig_input: &str,
    sig: &str,
    sig_key: &str,
    resolve_ap_key: impl Fn(&str) -> Result<Vec<u8>, String>,
) -> Result<String, String> {
    // Signature-Key: sig=jwt;jwt="<agent token>"
    let jwt = sig_key
        .strip_prefix("sig=jwt;jwt=\"")
        .and_then(|r| r.strip_suffix('"'))
        .ok_or("Signature-Key is not the jwt scheme")?;

    // 2. Verify the agent token against the AP's JWKS.
    let (jwt_header, claims) = decode_jwt(jwt)?;
    if jwt_header.get("typ").and_then(Value::as_str) != Some("aa-agent+jwt") {
        return Err("typ != aa-agent+jwt".into());
    }
    let kid = jwt_header
        .get("kid")
        .and_then(Value::as_str)
        .ok_or("no kid")?;
    let ap_key = resolve_ap_key(kid)?;
    verify_jwt_sig(jwt, &ap_key)?;
    let exp = claims.get("exp").and_then(Value::as_i64).unwrap_or(0);
    if exp < now() {
        return Err("agent token expired".into());
    }
    let agent = claims
        .get("sub")
        .and_then(Value::as_str)
        .ok_or("no sub")?
        .to_string();
    let cnf_x = claims
        .pointer("/cnf/jwk/x")
        .and_then(Value::as_str)
        .ok_or("no cnf.jwk.x")?;
    let cnf_key = b64url_decode(cnf_x)?;

    // 3. Reconstruct the signature base and verify with cnf.jwk (PoP).
    // Signature-Input: sig=("@method" "@authority" "@path" "signature-key");created=N
    let params = sig_input
        .strip_prefix("sig=")
        .ok_or("Signature-Input label != sig")?;
    let created: i64 = params
        .split("created=")
        .nth(1)
        .and_then(|c| c.split(';').next())
        .and_then(|c| c.trim().parse().ok())
        .ok_or("no created")?;
    if (now() - created).abs() > 300 {
        return Err("created outside window".into());
    }
    let components = params
        .strip_prefix('(')
        .and_then(|p| p.split(')').next())
        .ok_or("malformed component list")?;
    let mut base = String::new();
    for comp in components.split_whitespace() {
        let name = comp.trim_matches('"');
        let value = match name {
            "@method" => "POST".to_string(),
            "@authority" => headers
                .get("host")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_ascii_lowercase()
                .trim_end_matches(":443")
                .to_string(),
            "@path" => "/mcp".to_string(),
            "signature-key" => sig_key.to_string(),
            "content-digest" => header(headers, "content-digest").unwrap_or_default(),
            other => return Err(format!("unsupported covered component {other}")),
        };
        base.push_str(&format!("\"{name}\": {value}\n"));
    }
    base.push_str(&format!("\"@signature-params\": {params}"));

    // Signature: sig=:<std base64, padded>:
    let sig_b64 = sig
        .strip_prefix("sig=:")
        .and_then(|r| r.strip_suffix(':'))
        .ok_or("malformed Signature")?;
    let sig_bytes = b64std_decode(sig_b64)?;
    ed25519_verify(&cnf_key, base.as_bytes(), &sig_bytes)
        .map_err(|_| "HTTP signature does not verify against cnf.jwk".to_string())?;
    Ok(agent)
}

impl App {
    /// Fetch the AP's JWKS (metadata → jwks_uri) and return the Ed25519 `x` of
    /// key `kid`. Plain-HTTP in-cluster fetch, done per request — a fixture,
    /// not a cache-correct verifier.
    fn jwks_key(&self, kid: &str) -> Result<Vec<u8>, String> {
        let meta: Value =
            serde_json::from_slice(&http_get(&self.apd_host, "/.well-known/aauth-agent.json")?)
                .map_err(|e| format!("metadata json: {e}"))?;
        let jwks_uri = meta
            .get("jwks_uri")
            .and_then(Value::as_str)
            .ok_or("no jwks_uri")?;
        // Same-host path fetch (the e2e AP serves its own JWKS).
        let path = jwks_uri
            .splitn(4, '/')
            .nth(3)
            .map(|p| format!("/{p}"))
            .ok_or("jwks_uri has no path")?;
        let jwks: Value = serde_json::from_slice(&http_get(&self.apd_host, &path)?)
            .map_err(|e| format!("jwks json: {e}"))?;
        let keys = jwks
            .get("keys")
            .and_then(Value::as_array)
            .ok_or("no keys")?;
        for k in keys {
            if k.get("kid").and_then(Value::as_str) == Some(kid) {
                let x = k.get("x").and_then(Value::as_str).ok_or("key has no x")?;
                return b64url_decode(x);
            }
        }
        Err(format!("kid {kid} not in JWKS"))
    }
}

/// Minimal plaintext HTTP/1.1 GET (in-cluster, e2e-only).
fn http_get(host_port: &str, path: &str) -> Result<Vec<u8>, String> {
    let mut stream =
        std::net::TcpStream::connect(host_port).map_err(|e| format!("connect {host_port}: {e}"))?;
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .ok();
    let host = host_port.split(':').next().unwrap_or(host_port);
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nAccept: application/json\r\nConnection: close\r\n\r\n"
    )
    .map_err(|e| format!("write: {e}"))?;
    let mut buf = Vec::new();
    stream
        .read_to_end(&mut buf)
        .map_err(|e| format!("read: {e}"))?;
    let text = String::from_utf8_lossy(&buf);
    let (head, body) = text
        .split_once("\r\n\r\n")
        .ok_or("malformed HTTP response")?;
    if !head.contains(" 200 ") {
        return Err(format!("GET {path}: {}", head.lines().next().unwrap_or("")));
    }
    // Tolerate chunked encoding by stripping chunk-size lines if present.
    if head
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked")
    {
        let mut out = String::new();
        let mut rest = body;
        while let Some((size, tail)) = rest.split_once("\r\n") {
            let n = usize::from_str_radix(size.trim(), 16).unwrap_or(0);
            if n == 0 {
                break;
            }
            out.push_str(tail.get(..n).unwrap_or(""));
            rest = tail.get(n + 2..).unwrap_or("");
        }
        return Ok(out.into_bytes());
    }
    Ok(body.as_bytes().to_vec())
}

// --- JWT / crypto helpers (fixture-grade) -----------------------------------

fn decode_jwt(jwt: &str) -> Result<(Value, Value), String> {
    let mut parts = jwt.split('.');
    let h = parts.next().ok_or("jwt: no header")?;
    let p = parts.next().ok_or("jwt: no payload")?;
    let header: Value =
        serde_json::from_slice(&b64url_decode(h)?).map_err(|e| format!("jwt header: {e}"))?;
    let claims: Value =
        serde_json::from_slice(&b64url_decode(p)?).map_err(|e| format!("jwt payload: {e}"))?;
    Ok((header, claims))
}

fn verify_jwt_sig(jwt: &str, key: &[u8]) -> Result<(), String> {
    let (signed, sig) = jwt.rsplit_once('.').ok_or("jwt: no signature")?;
    let sig = b64url_decode(sig)?;
    ed25519_verify(key, signed.as_bytes(), &sig)
        .map_err(|_| "agent token does not verify against the AP JWKS".to_string())
}

fn ed25519_verify(key: &[u8], msg: &[u8], sig: &[u8]) -> Result<(), ()> {
    ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, key)
        .verify(msg, sig)
        .map_err(|_| ())
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn b64url_decode(s: &str) -> Result<Vec<u8>, String> {
    b64_decode(s, true)
}
fn b64std_decode(s: &str) -> Result<Vec<u8>, String> {
    b64_decode(s, false)
}
fn b64_decode(s: &str, url: bool) -> Result<Vec<u8>, String> {
    let mut acc: u32 = 0;
    let mut bits = 0u8;
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'-' if url => 62,
            b'_' if url => 63,
            b'+' if !url => 62,
            b'/' if !url => 63,
            b'=' => continue,
            _ => return Err(format!("invalid base64 byte {c:#x}")),
        };
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::signature::{Ed25519KeyPair, KeyPair};

    fn b64url(data: &[u8]) -> String {
        const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut out = String::new();
        for c in data.chunks(3) {
            let b = [c[0], *c.get(1).unwrap_or(&0), *c.get(2).unwrap_or(&0)];
            let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
            out.push(A[(n >> 18) as usize & 63] as char);
            out.push(A[(n >> 12) as usize & 63] as char);
            if c.len() > 1 {
                out.push(A[(n >> 6) as usize & 63] as char);
            }
            if c.len() > 2 {
                out.push(A[n as usize & 63] as char);
            }
        }
        out
    }
    fn b64std(data: &[u8]) -> String {
        const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for c in data.chunks(3) {
            let b = [c[0], *c.get(1).unwrap_or(&0), *c.get(2).unwrap_or(&0)];
            let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
            out.push(A[(n >> 18) as usize & 63] as char);
            out.push(A[(n >> 12) as usize & 63] as char);
            out.push(if c.len() > 1 {
                A[(n >> 6) as usize & 63] as char
            } else {
                '='
            });
            out.push(if c.len() > 2 {
                A[n as usize & 63] as char
            } else {
                '='
            });
        }
        out
    }
    fn kp(seed: &[u8; 32]) -> Ed25519KeyPair {
        Ed25519KeyPair::from_seed_unchecked(seed).unwrap()
    }
    fn jwt(header: &Value, claims: &Value, signer: &Ed25519KeyPair) -> String {
        let h = b64url(header.to_string().as_bytes());
        let p = b64url(claims.to_string().as_bytes());
        let signing_input = format!("{h}.{p}");
        let sig = signer.sign(signing_input.as_bytes());
        format!("{signing_input}.{}", b64url(sig.as_ref()))
    }

    /// Build a request exactly the way agentd's `aauth::sig` does, verify it end
    /// to end, then prove tampering is caught. This is the fixture's ground
    /// truth: the RFC 9421 base reconstruction + Ed25519 PoP + JWT/JWKS trust.
    #[test]
    fn verifies_an_agentd_style_signed_request_and_rejects_tampering() {
        // AP signing key (its JWKS) + the agent's ephemeral key (cnf.jwk).
        let ap = kp(&[7u8; 32]);
        let eph = kp(&[9u8; 32]);
        let eph_x = b64url(eph.public_key().as_ref());

        // aa-agent+jwt: header kid, typ; claims sub + cnf.jwk + exp.
        let now = now();
        let header = json!({ "typ": "aa-agent+jwt", "alg": "EdDSA", "kid": "ap-1" });
        let claims = json!({
            "iss": "http://apd.default.svc.cluster.local",
            "sub": "aauth:k7q3p9n2@apd.default.svc.cluster.local",
            "cnf": { "jwk": { "kty": "OKP", "crv": "Ed25519", "x": eph_x } },
            "exp": now + 300, "iat": now,
        });
        let token = jwt(&header, &claims, &ap);
        let sig_key = format!("sig=jwt;jwt=\"{token}\"");

        // RFC 9421: cover @method @authority @path signature-key.
        let params =
            format!("(\"@method\" \"@authority\" \"@path\" \"signature-key\");created={now}");
        let sig_input = format!("sig={params}");
        let authority = "mock-aauth-mcp.default.svc.cluster.local";
        let base = format!(
            "\"@method\": POST\n\"@authority\": {authority}\n\"@path\": /mcp\n\"signature-key\": {sig_key}\n\"@signature-params\": {params}"
        );
        let sig = format!("sig=:{}:", b64std(eph.sign(base.as_bytes()).as_ref()));

        let mut headers = HeaderMap::new();
        headers.insert("host", authority.parse().unwrap());
        let resolve = |kid: &str| {
            assert_eq!(kid, "ap-1");
            Ok(ap.public_key().as_ref().to_vec())
        };

        // Happy path: the verified principal is the token's sub.
        let agent = verify(&headers, &sig_input, &sig, &sig_key, resolve).unwrap();
        assert_eq!(agent, "aauth:k7q3p9n2@apd.default.svc.cluster.local");

        // Tamper 1: a different HTTP signature (wrong ephemeral key) is caught
        // by the cnf.jwk PoP check.
        let evil = kp(&[1u8; 32]);
        let bad_sig = format!("sig=:{}:", b64std(evil.sign(base.as_bytes()).as_ref()));
        assert!(verify(&headers, &sig_input, &bad_sig, &sig_key, resolve).is_err());

        // Tamper 2: a token signed by a NON-AP key is caught by the JWKS check.
        let forged = jwt(&header, &claims, &evil);
        let forged_key = format!("sig=jwt;jwt=\"{forged}\"");
        // (re-sign the base so the PoP would otherwise pass — isolate the JWKS check)
        let forged_base = base.replace(&sig_key, &forged_key);
        let forged_sig = format!(
            "sig=:{}:",
            b64std(eph.sign(forged_base.as_bytes()).as_ref())
        );
        assert!(verify(&headers, &sig_input, &forged_sig, &forged_key, resolve).is_err());

        // Tamper 3: an expired token is rejected.
        let expired_claims = json!({
            "iss": "x", "sub": "aauth:x@y",
            "cnf": { "jwk": { "kty": "OKP", "crv": "Ed25519", "x": eph_x } },
            "exp": now - 10,
        });
        let expired = jwt(&header, &expired_claims, &ap);
        let ekey = format!("sig=jwt;jwt=\"{expired}\"");
        let ebase = base.replace(&sig_key, &ekey);
        let esig = format!("sig=:{}:", b64std(eph.sign(ebase.as_bytes()).as_ref()));
        let err = verify(&headers, &sig_input, &esig, &ekey, resolve).unwrap_err();
        assert!(err.contains("expired"), "got: {err}");
    }
}
