// SPDX-License-Identifier: BUSL-1.1
//! TRUSTED-PROXY mode for the A2A gateway (front-proxy trust over mTLS).
//!
//! Mirrors the aggregated apiserver's front-proxy pattern, serving a dual
//! (plaintext + mTLS) listener. A fronting HTTP/API gateway (e.g. APISIX) terminates edge auth
//! and asserts the caller identity; agentctl trusts that assertion ONLY over an
//! authenticated mTLS channel.
//!
//! Gated on `TRUSTED_PROXY_ENABLED` (default OFF; when off the gateway serves only
//! the plaintext listener). When ON the gateway serves a SECOND listener:
//!
//! * **`:8080` plaintext** — the plaintext surface. UNTRUSTED: every inbound
//!   trusted-proxy identity header (the configured `<prefix>-subject/-email/-groups`,
//!   default prefix `x-agentctl`, plus the legacy `X-Forwarded-*` set) is STRIPPED
//!   before handling, so an in-cluster caller can never self-assert identity
//!   ([`strip_plaintext`]).
//! * **`AGENTCTL_GATEWAY_TLS_ADDR` (default `:8443`) mTLS** — rustls REQUIRES a
//!   client cert chained to the trusted-proxy CA (`TRUSTED_PROXY_CA`) via a
//!   [`WebPkiClientVerifier`]. After the chain verifies, the peer cert's CN/SAN
//!   must additionally be in `TRUSTED_PROXY_ALLOWED_NAMES` ([`mtls_decision`]) —
//!   else 403. The peer cert is captured off the TLS connection by
//!   [`PeerCertAcceptor`] (a custom `axum_server::accept::Accept`). A request that
//!   passes carrying the identity headers yields a TRUSTED [`Decision`].
//!
//! ring only (no openssl/aws-lc) — the process installs the ring provider in
//! `main`, so [`build_tls_config`] uses the plain `ServerConfig`/`WebPkiClientVerifier`
//! builders like the apiserver.

use std::future::Future;
use std::io::{self, BufReader};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use axum::extract::{FromRequestParts, Request, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::{AddExtension, Next};
use axum::response::{IntoResponse, Response};
use axum::Extension;
use axum_server::accept::Accept;
use axum_server::tls_rustls::{RustlsAcceptor, RustlsConfig};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::WebPkiClientVerifier;
use rustls::{RootCertStore, ServerConfig};
use serde_json::{json, Value};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_rustls::server::TlsStream;
use tower::Layer;

use crate::metrics::Metrics;
use crate::oidc::Identity;

/// Default bind for the mTLS front-proxy listener.
const DEFAULT_TLS_ADDR: &str = "0.0.0.0:8443";
/// Default mount of the gateway's serving cert/key for the mTLS listener.
const DEFAULT_TLS_DIR: &str = "/etc/agentctl-gateway/tls";
/// Default mount of the trusted-proxy CA (verifies the proxy's client cert).
const DEFAULT_CA_PATH: &str = "/etc/agentctl-trusted-proxy/ca.crt";

/// Default prefix for the identity headers the trusted proxy asserts; the names
/// derive as `<prefix>-subject`, `<prefix>-email`, `<prefix>-groups` (configurable
/// via `TRUSTED_PROXY_HEADER_PREFIX`).
const DEFAULT_HEADER_PREFIX: &str = "x-agentctl";

/// Legacy header names ALSO stripped from plaintext callers (belt-and-suspenders):
/// a common proxy convention that must never be self-asserted on :8080, regardless
/// of the configured prefix.
const LEGACY_USER_HEADER: &str = "X-Forwarded-User";
const LEGACY_EMAIL_HEADER: &str = "X-Forwarded-Email";
const LEGACY_GROUPS_HEADER: &str = "X-Forwarded-Groups";

/// X.509 OIDs we read off the verified peer cert (dotted form; `new_unwrap` is a
/// compile-time const). CN (subject) and SubjectAltName.
const CN_OID: x509_cert::der::asn1::ObjectIdentifier =
    x509_cert::der::asn1::ObjectIdentifier::new_unwrap("2.5.4.3");
const SAN_OID: x509_cert::der::asn1::ObjectIdentifier =
    x509_cert::der::asn1::ObjectIdentifier::new_unwrap("2.5.29.17");

/// The identity header names the trusted proxy asserts (configurable via
/// `TRUSTED_PROXY_IDENTITY_HEADERS`, e.g. `user=X-Forwarded-User,...`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IdentityHeaders {
    pub user: String,
    pub email: String,
    pub groups: String,
}

impl IdentityHeaders {
    /// Derive the asserted header names from a prefix: `<prefix>-subject`,
    /// `<prefix>-email`, `<prefix>-groups`. A trailing `-` on the prefix is ignored.
    pub fn from_prefix(prefix: &str) -> Self {
        let p = prefix.trim().trim_end_matches('-');
        Self {
            user: format!("{p}-subject"),
            email: format!("{p}-email"),
            groups: format!("{p}-groups"),
        }
    }

    /// Apply explicit `user=..,email=..,groups=..` overrides onto self; unspecified
    /// keys keep their (prefix-derived) value.
    pub fn with_overrides(mut self, spec: &str) -> Self {
        for part in spec.split(',') {
            let Some((k, v)) = part.split_once('=') else {
                continue;
            };
            let v = v.trim();
            if v.is_empty() {
                continue;
            }
            match k.trim().to_ascii_lowercase().as_str() {
                "user" => self.user = v.to_string(),
                "email" => self.email = v.to_string(),
                "groups" => self.groups = v.to_string(),
                _ => {}
            }
        }
        self
    }
}

impl Default for IdentityHeaders {
    fn default() -> Self {
        Self::from_prefix(DEFAULT_HEADER_PREFIX)
    }
}

/// Trusted-proxy configuration, parsed once at startup.
#[derive(Clone, Debug)]
pub struct Config {
    /// `TRUSTED_PROXY_ENABLED` — OFF by default.
    pub enabled: bool,
    /// `AGENTCTL_GATEWAY_TLS_ADDR` — the mTLS listener bind.
    pub tls_addr: String,
    /// `AGENTCTL_GATEWAY_TLS_DIR` — the gateway's serving `tls.crt`/`tls.key`.
    pub tls_dir: PathBuf,
    /// `TRUSTED_PROXY_CA` — the CA the proxy's client cert must chain to.
    pub ca_path: PathBuf,
    /// `TRUSTED_PROXY_ALLOWED_NAMES` — accepted client-cert CN/SAN values.
    pub allowed_names: Vec<String>,
    /// The asserted identity header names.
    pub identity_headers: IdentityHeaders,
}

impl Config {
    /// Parse the trusted-proxy config from the environment.
    pub fn from_env() -> Self {
        let enabled = std::env::var("TRUSTED_PROXY_ENABLED")
            .map(|v| is_truthy(&v))
            .unwrap_or(false);
        let tls_addr =
            std::env::var("AGENTCTL_GATEWAY_TLS_ADDR").unwrap_or_else(|_| DEFAULT_TLS_ADDR.into());
        let tls_dir = std::env::var("AGENTCTL_GATEWAY_TLS_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(DEFAULT_TLS_DIR));
        let ca_path = std::env::var("TRUSTED_PROXY_CA")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(DEFAULT_CA_PATH));
        let allowed_names =
            parse_allowed_names(&std::env::var("TRUSTED_PROXY_ALLOWED_NAMES").unwrap_or_default());
        // Header names derive from a configurable prefix (TRUSTED_PROXY_HEADER_PREFIX
        // -> <prefix>-subject/-email/-groups); an explicit TRUSTED_PROXY_IDENTITY_HEADERS
        // (user=..,email=..,groups=..) overrides individual names on top.
        let prefix = std::env::var("TRUSTED_PROXY_HEADER_PREFIX")
            .unwrap_or_else(|_| DEFAULT_HEADER_PREFIX.into());
        let mut identity_headers = IdentityHeaders::from_prefix(&prefix);
        if let Ok(spec) = std::env::var("TRUSTED_PROXY_IDENTITY_HEADERS") {
            identity_headers = identity_headers.with_overrides(&spec);
        }
        Self {
            enabled,
            tls_addr,
            tls_dir,
            ca_path,
            allowed_names,
            identity_headers,
        }
    }
}

/// The per-request trust decision threaded to the A2A handler as a request
/// extension. `Trusted` is formed only on the verified mTLS listener for an
/// allow-listed peer carrying the asserted identity headers.
#[derive(Clone, Debug)]
pub enum Decision {
    /// Plaintext caller, or an mTLS caller without asserted identity headers.
    Untrusted,
    /// A verified trusted-proxy caller with the asserted identity.
    Trusted(Identity),
}

/// Extractor for the per-request [`Decision`]. Defaults to [`Decision::Untrusted`]
/// when no decision was injected (e.g. trusted-proxy mode disabled), so the A2A
/// handler is robust on every listener.
pub struct TrustedDecision(pub Decision);

impl<S> FromRequestParts<S> for TrustedDecision
where
    S: Send + Sync,
{
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let decision = parts
            .extensions
            .get::<Decision>()
            .cloned()
            .unwrap_or(Decision::Untrusted);
        Ok(TrustedDecision(decision))
    }
}

/// The verified peer's leaf certificate (DER), captured off the TLS connection by
/// [`PeerCertAcceptor`] and injected as a request extension on the mTLS listener.
#[derive(Clone)]
pub struct PeerCertDer(pub Option<Vec<u8>>);

/// State for the [`mtls_decision`] middleware: the trusted-proxy config + metrics.
#[derive(Clone)]
pub struct MtlsCtx {
    pub cfg: Arc<Config>,
    pub metrics: Arc<Metrics>,
}

// --- TLS server config (mirrors apiserver) ----------------------------------

/// rustls server config for the mTLS front-proxy listener: present the gateway's
/// serving cert AND **require** a client cert chained to the trusted-proxy CA (so
/// only the fronting proxy can reach this listener). Relies on the process-default
/// ring provider installed in `main` (like the apiserver).
pub fn build_tls_config(tls_dir: &Path, ca_path: &Path) -> Result<ServerConfig, String> {
    let certs = load_certs(&tls_dir.join("tls.crt"))?;
    let key = load_key(&tls_dir.join("tls.key"))?;
    let client_ca = load_ca(ca_path)?;
    let verifier = WebPkiClientVerifier::builder(Arc::new(client_ca))
        .build()
        .map_err(|e| format!("client verifier: {e}"))?;
    ServerConfig::builder()
        .with_client_cert_verifier(verifier)
        .with_single_cert(certs, key)
        .map_err(|e| format!("server config: {e}"))
}

fn load_certs(path: &Path) -> Result<Vec<CertificateDer<'static>>, String> {
    let mut r =
        BufReader::new(std::fs::File::open(path).map_err(|e| format!("open {path:?}: {e}"))?);
    rustls_pemfile::certs(&mut r)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("read certs {path:?}: {e}"))
}

fn load_key(path: &Path) -> Result<PrivateKeyDer<'static>, String> {
    let mut r =
        BufReader::new(std::fs::File::open(path).map_err(|e| format!("open {path:?}: {e}"))?);
    rustls_pemfile::private_key(&mut r)
        .map_err(|e| format!("read key {path:?}: {e}"))?
        .ok_or_else(|| format!("no private key in {path:?}"))
}

fn load_ca(path: &Path) -> Result<RootCertStore, String> {
    let mut r =
        BufReader::new(std::fs::File::open(path).map_err(|e| format!("open {path:?}: {e}"))?);
    let mut roots = RootCertStore::empty();
    for cert in rustls_pemfile::certs(&mut r) {
        roots
            .add(cert.map_err(|e| format!("parse CA {path:?}: {e}"))?)
            .map_err(|e| format!("add CA {path:?}: {e}"))?;
    }
    if roots.is_empty() {
        return Err(format!("CA file {path:?} had no certs"));
    }
    Ok(roots)
}

// --- custom acceptor: capture the verified peer cert ------------------------

/// A custom `axum_server::accept::Accept` that wraps the rustls acceptor and, once
/// the handshake completes, captures the verified peer's leaf cert off the TLS
/// connection and injects it as a [`PeerCertDer`] request extension (mirrors the
/// upstream `rustls_session` example's [`ServerConnection`] access).
#[derive(Clone)]
pub struct PeerCertAcceptor {
    inner: RustlsAcceptor,
}

impl PeerCertAcceptor {
    /// Wrap a [`RustlsConfig`] requiring a client cert (see [`build_tls_config`]).
    pub fn new(config: RustlsConfig) -> Self {
        Self {
            inner: RustlsAcceptor::new(config),
        }
    }
}

impl<I, S> Accept<I, S> for PeerCertAcceptor
where
    I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    S: Send + 'static,
{
    type Stream = TlsStream<I>;
    type Service = AddExtension<S, PeerCertDer>;
    type Future = Pin<Box<dyn Future<Output = io::Result<(Self::Stream, Self::Service)>> + Send>>;

    fn accept(&self, stream: I, service: S) -> Self::Future {
        let acceptor = self.inner.clone();
        Box::pin(async move {
            let (stream, service) = acceptor.accept(stream, service).await?;
            // `get_ref().1` is the rustls `ServerConnection`; the chain is already
            // verified (WebPkiClientVerifier), so the leaf cert is the peer's.
            let leaf = stream
                .get_ref()
                .1
                .peer_certificates()
                .and_then(|chain| chain.first())
                .map(|c| c.as_ref().to_vec());
            let service = Extension(PeerCertDer(leaf)).layer(service);
            Ok((stream, service))
        })
    }
}

// --- middleware -------------------------------------------------------------

/// Plaintext (`:8080`) anti-spoof middleware: STRIP every trusted-proxy identity
/// header before any handling, so an untrusted in-cluster caller can never
/// self-assert identity. Only the mTLS listener may carry trusted identity.
pub async fn strip_plaintext(
    State(cfg): State<Arc<Config>>,
    mut req: Request,
    next: Next,
) -> Response {
    strip_identity_headers(req.headers_mut(), &cfg.identity_headers);
    next.run(req).await
}

/// mTLS listener middleware: enforce the allow-list against the verified peer
/// cert's CN/SAN (403 on miss), then — when the asserted identity headers are
/// present — inject a [`Decision::Trusted`] for the A2A handler.
pub async fn mtls_decision(State(ctx): State<MtlsCtx>, mut req: Request, next: Next) -> Response {
    let names = req
        .extensions()
        .get::<PeerCertDer>()
        .and_then(|p| p.0.as_deref())
        .map(cert_names)
        .unwrap_or_default();
    if !name_allowed(&names, &ctx.cfg.allowed_names) {
        ctx.metrics.inc_trusted_proxy_rejected();
        tracing::warn!(
            ?names,
            "trusted-proxy: client-cert name not in allow-list; rejecting (403)"
        );
        return StatusCode::FORBIDDEN.into_response();
    }
    if let Some(identity) = extract_asserted_identity(req.headers(), &ctx.cfg.identity_headers) {
        req.extensions_mut().insert(Decision::Trusted(identity));
    }
    next.run(req).await
}

// --- pure helpers (unit-tested) ---------------------------------------------

/// Whether an env flag is truthy (`1`/`true`/`yes`/`on`, case-insensitive).
fn is_truthy(v: &str) -> bool {
    matches!(
        v.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// Parse the CSV allow-list of accepted client-cert CN/SAN values (trim; drop
/// empties).
pub fn parse_allowed_names(csv: &str) -> Vec<String> {
    csv.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Parse `TRUSTED_PROXY_IDENTITY_HEADERS` (`user=..,email=..,groups=..`) onto the
/// default (prefix-derived) names — a thin wrapper over
/// [`IdentityHeaders::with_overrides`]; `Config::from_env` applies overrides onto
/// the configured prefix directly.
#[cfg(test)]
pub fn parse_identity_headers(spec: &str) -> IdentityHeaders {
    IdentityHeaders::default().with_overrides(spec)
}

/// Extract the CN (subject) and every DNS SubjectAltName from a DER cert. A parse
/// failure yields an empty list (fail closed at the allow-list check).
pub fn cert_names(der: &[u8]) -> Vec<String> {
    use x509_cert::der::Decode;
    use x509_cert::ext::pkix::name::GeneralName;
    use x509_cert::ext::pkix::SubjectAltName;
    use x509_cert::Certificate;

    let mut names = Vec::new();
    let Ok(cert) = Certificate::from_der(der) else {
        return names;
    };

    // CN from the subject RDN sequence.
    for rdn in cert.tbs_certificate.subject.0.iter() {
        for atv in rdn.0.iter() {
            if atv.oid == CN_OID {
                // DirectoryString (Utf8/Printable/IA5) content bytes are the text.
                if let Ok(s) = std::str::from_utf8(atv.value.value()) {
                    names.push(s.to_string());
                }
            }
        }
    }

    // DNS SubjectAltName entries.
    if let Some(exts) = cert.tbs_certificate.extensions.as_ref() {
        for ext in exts.iter() {
            if ext.extn_id != SAN_OID {
                continue;
            }
            if let Ok(san) = SubjectAltName::from_der(ext.extn_value.as_bytes()) {
                for gn in san.0.iter() {
                    if let GeneralName::DnsName(dns) = gn {
                        names.push(dns.as_str().to_string());
                    }
                }
            }
        }
    }

    names
}

/// Whether any of the peer cert's `names` is in the `allowed` list (case-
/// insensitive). An empty allow-list fails closed (nothing is allowed) — the
/// operator MUST set `TRUSTED_PROXY_ALLOWED_NAMES`.
pub fn name_allowed(names: &[String], allowed: &[String]) -> bool {
    if allowed.is_empty() {
        return false;
    }
    names
        .iter()
        .any(|n| allowed.iter().any(|a| a.eq_ignore_ascii_case(n)))
}

/// Project the asserted caller [`Identity`] from the inbound headers. Returns
/// `None` when the user header is absent/empty (no asserted identity ⇒ the request
/// falls through to the normal auth precedence).
pub fn extract_asserted_identity(headers: &HeaderMap, h: &IdentityHeaders) -> Option<Identity> {
    let sub = header_value(headers, &h.user).filter(|s| !s.is_empty())?;
    let email = header_value(headers, &h.email).filter(|s| !s.is_empty());
    let groups = headers
        .get(h.groups.as_str())
        .and_then(|v| v.to_str().ok())
        .map(|v| {
            v.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    Some(Identity { sub, email, groups })
}

/// Remove every trusted-proxy identity header from `headers`: the configured names
/// AND the defaults (so a rename can't be bypassed by sending the default header).
pub fn strip_identity_headers(headers: &mut HeaderMap, h: &IdentityHeaders) {
    for name in [
        h.user.as_str(),
        h.email.as_str(),
        h.groups.as_str(),
        LEGACY_USER_HEADER,
        LEGACY_EMAIL_HEADER,
        LEGACY_GROUPS_HEADER,
    ] {
        headers.remove(name);
    }
}

/// Build a JSON claims object from an asserted [`Identity`] so the existing OIDC
/// `required_claims` enforcement ([`crate::oidc::enforce_claims`]) can authorize a
/// trusted-proxy caller against `sub`/`email`/`groups`.
pub fn identity_claims(id: &Identity) -> Value {
    json!({
        "sub": id.sub,
        "email": id.email,
        "groups": id.groups,
    })
}

/// One header value as an owned `String` (UTF-8 only).
fn header_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_api::ClaimRequirement;

    const TRUSTED_CA: &[u8] = include_bytes!("../testdata/trusted_proxy_ca.crt");
    const CLIENT_APISIX: &[u8] = include_bytes!("../testdata/client_apisix.crt");
    const CLIENT_WRONG_CA: &[u8] = include_bytes!("../testdata/client_wrong_ca.crt");

    fn der(pem: &[u8]) -> CertificateDer<'static> {
        rustls_pemfile::certs(&mut &pem[..])
            .next()
            .expect("a cert in the PEM")
            .expect("valid cert")
    }

    fn roots(pem: &[u8]) -> RootCertStore {
        let mut store = RootCertStore::empty();
        for c in rustls_pemfile::certs(&mut &pem[..]) {
            store.add(c.unwrap()).unwrap();
        }
        store
    }

    fn hv(map: &mut HeaderMap, name: &'static str, value: &str) {
        map.insert(name, value.parse().unwrap());
    }

    // --- TLS chain verification (good CA accept / wrong CA reject) ----------

    #[test]
    fn client_cert_chain_verifies_against_trusted_ca_and_rejects_wrong_ca() {
        use rustls::pki_types::UnixTime;
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let verifier =
            WebPkiClientVerifier::builder_with_provider(Arc::new(roots(TRUSTED_CA)), provider)
                .build()
                .expect("build client verifier");
        let now = UnixTime::now();

        // A cert signed by the trusted CA verifies.
        let good = der(CLIENT_APISIX);
        assert!(verifier.verify_client_cert(&good, &[], now).is_ok());

        // A cert signed by a DIFFERENT CA is rejected (unknown issuer).
        let bad = der(CLIENT_WRONG_CA);
        assert!(verifier.verify_client_cert(&bad, &[], now).is_err());
    }

    // --- peer-cert name extraction + allow-list ----------------------------

    #[test]
    fn cert_names_extracts_cn_and_dns_sans() {
        let names = cert_names(der(CLIENT_APISIX).as_ref());
        assert!(names.contains(&"apisix".to_string()), "CN: {names:?}");
        assert!(
            names.contains(&"apisix.agentctl-system.svc".to_string()),
            "SAN: {names:?}"
        );
    }

    #[test]
    fn allowed_names_in_list_accept_not_in_list_reject() {
        let names = cert_names(der(CLIENT_APISIX).as_ref());
        // In-list (matches CN) → accepted.
        assert!(name_allowed(&names, &["apisix".to_string()]));
        // In-list via SAN → accepted.
        assert!(name_allowed(
            &names,
            &["apisix.agentctl-system.svc".to_string()]
        ));
        // Case-insensitive.
        assert!(name_allowed(&names, &["APISIX".to_string()]));
        // Not-in-list → rejected (the middleware turns this into 403).
        assert!(!name_allowed(&names, &["envoy".to_string()]));
        // Empty allow-list fails closed.
        assert!(!name_allowed(&names, &[]));
    }

    // --- env parsing -------------------------------------------------------

    #[test]
    fn parse_allowed_names_trims_and_drops_empties() {
        assert_eq!(parse_allowed_names(""), Vec::<String>::new());
        assert_eq!(
            parse_allowed_names(" apisix , envoy ,, "),
            vec!["apisix".to_string(), "envoy".to_string()]
        );
    }

    #[test]
    fn parse_identity_headers_overrides_only_present_keys() {
        // Full custom spec.
        let h =
            parse_identity_headers("user=X-Edge-User, email=X-Edge-Email, groups=X-Edge-Groups");
        assert_eq!(h.user, "X-Edge-User");
        assert_eq!(h.email, "X-Edge-Email");
        assert_eq!(h.groups, "X-Edge-Groups");

        // Partial spec keeps the (prefix-derived) defaults for unspecified keys.
        let h2 = parse_identity_headers("user=X-Only-User");
        assert_eq!(h2.user, "X-Only-User");
        assert_eq!(h2.email, "x-agentctl-email");
        assert_eq!(h2.groups, "x-agentctl-groups");

        // Empty spec ⇒ all defaults.
        assert_eq!(parse_identity_headers(""), IdentityHeaders::default());
    }

    #[test]
    fn header_names_derive_from_prefix() {
        // Default prefix.
        let d = IdentityHeaders::default();
        assert_eq!(d.user, "x-agentctl-subject");
        assert_eq!(d.email, "x-agentctl-email");
        assert_eq!(d.groups, "x-agentctl-groups");

        // Custom prefix (trailing '-' tolerated).
        let h = IdentityHeaders::from_prefix("x-acme-");
        assert_eq!(h.user, "x-acme-subject");
        assert_eq!(h.email, "x-acme-email");
        assert_eq!(h.groups, "x-acme-groups");

        // Prefix + an explicit per-key override compose.
        let h2 = IdentityHeaders::from_prefix("x-acme").with_overrides("groups=X-Acme-Roles");
        assert_eq!(h2.user, "x-acme-subject");
        assert_eq!(h2.groups, "X-Acme-Roles");
    }

    // --- identity-header extraction ----------------------------------------

    #[test]
    fn extract_identity_from_default_headers() {
        let mut h = HeaderMap::new();
        hv(&mut h, "x-agentctl-subject", "alice");
        hv(&mut h, "x-agentctl-email", "alice@corp.example");
        hv(&mut h, "x-agentctl-groups", "eng, oncall");
        let id = extract_asserted_identity(&h, &IdentityHeaders::default()).expect("identity");
        assert_eq!(id.sub, "alice");
        assert_eq!(id.email.as_deref(), Some("alice@corp.example"));
        assert_eq!(id.groups, vec!["eng".to_string(), "oncall".to_string()]);
    }

    #[test]
    fn extract_identity_none_without_user_header() {
        let mut h = HeaderMap::new();
        hv(&mut h, "x-agentctl-groups", "eng");
        assert!(extract_asserted_identity(&h, &IdentityHeaders::default()).is_none());
    }

    #[test]
    fn extract_identity_honours_configured_header_names() {
        let cfg =
            parse_identity_headers("user=X-Edge-User,email=X-Edge-Email,groups=X-Edge-Groups");
        let mut h = HeaderMap::new();
        hv(&mut h, "X-Edge-User", "bob");
        hv(&mut h, "X-Edge-Groups", "admins");
        // A default-named header is ignored when a custom name is configured.
        hv(&mut h, "X-Forwarded-User", "spoofed");
        let id = extract_asserted_identity(&h, &cfg).expect("identity");
        assert_eq!(id.sub, "bob");
        assert_eq!(id.groups, vec!["admins".to_string()]);
    }

    // --- anti-spoof strip --------------------------------------------------

    #[test]
    fn strip_removes_default_identity_headers_keeps_others() {
        let mut h = HeaderMap::new();
        hv(&mut h, "X-Forwarded-User", "evil");
        hv(&mut h, "X-Forwarded-Email", "evil@corp.example");
        hv(&mut h, "X-Forwarded-Groups", "admins");
        hv(&mut h, "Authorization", "Bearer keep-me");
        hv(&mut h, "X-Custom", "keep");
        strip_identity_headers(&mut h, &IdentityHeaders::default());
        assert!(h.get("X-Forwarded-User").is_none());
        assert!(h.get("X-Forwarded-Email").is_none());
        assert!(h.get("X-Forwarded-Groups").is_none());
        // Non-identity headers are untouched.
        assert_eq!(h.get("Authorization").unwrap(), "Bearer keep-me");
        assert_eq!(h.get("X-Custom").unwrap(), "keep");
    }

    #[test]
    fn strip_removes_both_configured_and_default_names() {
        let cfg = parse_identity_headers("user=X-Edge-User");
        let mut h = HeaderMap::new();
        hv(&mut h, "X-Edge-User", "evil"); // configured name
        hv(&mut h, "X-Forwarded-User", "evil"); // default name still stripped
        strip_identity_headers(&mut h, &cfg);
        assert!(h.get("X-Edge-User").is_none());
        assert!(h.get("X-Forwarded-User").is_none());
    }

    // --- requiredClaims against asserted identity --------------------------

    #[test]
    fn required_claims_enforced_against_asserted_groups() {
        let id = Identity {
            sub: "alice".to_string(),
            email: None,
            groups: vec!["eng".to_string(), "oncall".to_string()],
        };
        let claims = identity_claims(&id);

        // Satisfied: caller is in `oncall`.
        let ok = vec![ClaimRequirement {
            claim: "groups".to_string(),
            any_of: vec!["oncall".to_string(), "admins".to_string()],
        }];
        assert!(crate::oidc::enforce_claims(&claims, Some(&ok)).is_ok());

        // Unsatisfied: caller is not in `admins` → 403 (Forbidden).
        let deny = vec![ClaimRequirement {
            claim: "groups".to_string(),
            any_of: vec!["admins".to_string()],
        }];
        assert!(crate::oidc::enforce_claims(&claims, Some(&deny)).is_err());
    }
}
