// SPDX-License-Identifier: BUSL-1.1
//! OPTIONAL mTLS listener for the coordination MCP server.
//!
//! Internal callers (the KEDA scaler reading `work.stats`) authenticate with a
//! CA-signed CLIENT CERTIFICATE instead of the coarse, shared `AGENTCTL_API_TOKEN`.
//! Mirrors the gateway's trusted-proxy listener: a rustls server that REQUIRES a
//! client cert chained to a configured CA, with an additional CN/SAN allow-list on
//! top of chain verification.
//!
//! Gated on `COORDINATION_MTLS_ADDR` being set (default UNSET ⇒ OFF; off ⇒ no
//! second listener, no cluster reads, no crypto provider install). When set the
//! server serves a SECOND listener ALONGSIDE the plaintext `:8080` (`tokio::join!`):
//!
//! * **`:8080` plaintext** — the primary surface. The `AGENTCTL_API_TOKEN` bearer
//!   gate applies here.
//! * **`COORDINATION_MTLS_ADDR` (e.g. `0.0.0.0:8443`) mTLS** — rustls presents the
//!   serving cert/key from `COORDINATION_MTLS_DIR` (default
//!   `/etc/agentctl-coordination-mtls`)`/{tls.crt,tls.key}` and **requires** a
//!   client cert chained to `COORDINATION_MTLS_CA` (default
//!   `/etc/agentctl-coordination-mtls/ca.crt`) via a [`WebPkiClientVerifier`].
//!   After the chain verifies, the peer cert's CN/DNS-SAN must additionally be in
//!   `COORDINATION_MTLS_ALLOWED_NAMES` ([`name_allowed`]) — else 403. The peer cert
//!   is captured off the TLS connection by [`PeerCertAcceptor`]. A verified +
//!   allow-listed cert IS the authentication, so the bearer gate is SKIPPED for
//!   these connections (the gate sees the [`MtlsVerified`] marker and passes). The
//!   SAME MCP routes (`work.*`) are served.
//!
//! ring only (no openssl/aws-lc): when enabled the binary installs the process
//! default ring provider, so [`build_tls_config`] uses the plain
//! `ServerConfig`/`WebPkiClientVerifier` builders (like the gateway).
//! Missing/invalid material ⇒ panic at startup (caller `expect`s [`build_tls_config`]).

use std::future::Future;
use std::io::{self, BufReader};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::{AddExtension, Next};
use axum::response::{IntoResponse, Response};
use axum::Extension;
use axum_server::accept::Accept;
use axum_server::tls_rustls::{RustlsAcceptor, RustlsConfig};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::WebPkiClientVerifier;
use rustls::{RootCertStore, ServerConfig};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_rustls::server::TlsStream;
use tower::Layer;

use crate::metrics::Metrics;

/// Default mount of the coordination server's mTLS material (serving cert/key + CA).
const DEFAULT_MTLS_DIR: &str = "/etc/agentctl-coordination-mtls";

/// X.509 OIDs we read off the verified peer cert (dotted form; `new_unwrap` is a
/// compile-time const). CN (subject) and SubjectAltName.
const CN_OID: x509_cert::der::asn1::ObjectIdentifier =
    x509_cert::der::asn1::ObjectIdentifier::new_unwrap("2.5.4.3");
const SAN_OID: x509_cert::der::asn1::ObjectIdentifier =
    x509_cert::der::asn1::ObjectIdentifier::new_unwrap("2.5.29.17");

/// mTLS listener configuration, parsed once at startup. Constructed only when the
/// feature is enabled ([`Config::from_env`] returns `None` when
/// `COORDINATION_MTLS_ADDR` is unset/empty).
#[derive(Clone, Debug)]
pub struct Config {
    /// `COORDINATION_MTLS_ADDR` — the mTLS listener bind (e.g. `0.0.0.0:8443`).
    pub addr: String,
    /// `COORDINATION_MTLS_DIR` — the serving `tls.crt`/`tls.key` directory.
    pub tls_dir: PathBuf,
    /// `COORDINATION_MTLS_CA` — the CA the client cert must chain to.
    pub ca_path: PathBuf,
    /// `COORDINATION_MTLS_ALLOWED_NAMES` — accepted client-cert CN/SAN values.
    pub allowed_names: Vec<String>,
}

impl Config {
    /// Parse the mTLS config from the environment. Returns `None` when
    /// `COORDINATION_MTLS_ADDR` is unset/empty (the feature is OFF).
    pub fn from_env() -> Option<Self> {
        let addr = std::env::var("COORDINATION_MTLS_ADDR")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())?;
        let tls_dir = std::env::var("COORDINATION_MTLS_DIR")
            .ok()
            .map(|v| PathBuf::from(v.trim()))
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| PathBuf::from(DEFAULT_MTLS_DIR));
        let ca_path = std::env::var("COORDINATION_MTLS_CA")
            .ok()
            .map(|v| PathBuf::from(v.trim()))
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| tls_dir.join("ca.crt"));
        let allowed_names = parse_allowed_names(
            &std::env::var("COORDINATION_MTLS_ALLOWED_NAMES").unwrap_or_default(),
        );
        Some(Self {
            addr,
            tls_dir,
            ca_path,
            allowed_names,
        })
    }
}

/// Marker injected by [`mtls_gate`] onto a request that arrived on the mTLS
/// listener with a verified + allow-listed client cert. The bearer-token gate
/// treats its presence as proof of authentication and SKIPS the token check (the
/// client cert IS the authentication).
#[derive(Clone, Copy)]
pub struct MtlsVerified;

/// The verified peer's leaf certificate (DER), captured off the TLS connection by
/// [`PeerCertAcceptor`] and injected as a request extension on the mTLS listener.
#[derive(Clone)]
pub struct PeerCertDer(pub Option<Vec<u8>>);

/// State for the [`mtls_gate`] middleware: the mTLS config + metrics.
#[derive(Clone)]
pub struct MtlsCtx {
    pub cfg: Arc<Config>,
    pub metrics: Arc<Metrics>,
}

// --- TLS server config (mirrors the gateway) --------------------------------

/// rustls server config for the mTLS listener: present the coordination server's
/// serving cert AND **require** a client cert chained to the configured CA (so only
/// allow-listed internal callers can reach it). Relies on the process-default ring
/// provider installed by the caller when the feature is enabled.
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
/// connection and injects it as a [`PeerCertDer`] request extension. Mirrors the
/// gateway's trusted-proxy acceptor.
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

/// mTLS listener gate: enforce the allow-list against the verified peer cert's
/// CN/SAN. On a miss, count a rejection and return 403. On a match, count an
/// acceptance and inject the [`MtlsVerified`] marker so the downstream bearer gate
/// skips the `AGENTCTL_API_TOKEN` check (the client cert IS the authentication).
///
/// The TLS chain is already verified by the rustls `WebPkiClientVerifier` before
/// any request reaches here — a cert chained to the wrong CA never completes the
/// handshake, so it is rejected at the transport layer (not counted here).
pub async fn mtls_gate(State(ctx): State<MtlsCtx>, mut req: Request, next: Next) -> Response {
    let names = req
        .extensions()
        .get::<PeerCertDer>()
        .and_then(|p| p.0.as_deref())
        .map(cert_names)
        .unwrap_or_default();
    if !name_allowed(&names, &ctx.cfg.allowed_names) {
        ctx.metrics.inc_mtls_rejected();
        tracing::warn!(
            ?names,
            "coordination mTLS: client-cert name not in allow-list; rejecting (403)"
        );
        return StatusCode::FORBIDDEN.into_response();
    }
    ctx.metrics.inc_mtls_accepted();
    req.extensions_mut().insert(MtlsVerified);
    next.run(req).await
}

// --- pure helpers (unit-tested) ---------------------------------------------

/// Parse the CSV allow-list of accepted client-cert CN/SAN values (trim; drop
/// empties).
pub fn parse_allowed_names(csv: &str) -> Vec<String> {
    csv.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Extract the CN (subject) and every DNS SubjectAltName from a DER cert. A parse
/// failure yields an empty list (fail closed at the allow-list check). Mirrors the
/// gateway's trusted-proxy extraction.
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
/// operator MUST set `COORDINATION_MTLS_ALLOWED_NAMES`.
pub fn name_allowed(names: &[String], allowed: &[String]) -> bool {
    if allowed.is_empty() {
        return false;
    }
    names
        .iter()
        .any(|n| allowed.iter().any(|a| a.eq_ignore_ascii_case(n)))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TRUSTED_CA: &[u8] = include_bytes!("../testdata/coordination_mtls_ca.crt");
    const CLIENT_SCALER: &[u8] = include_bytes!("../testdata/client_scaler.crt");
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

    // --- TLS chain verification (good CA accept / wrong CA reject) ----------

    #[test]
    fn client_cert_chain_verifies_against_trusted_ca_and_rejects_wrong_ca() {
        use rustls::pki_types::UnixTime;
        // builder_with_provider so the test never depends on a process-wide
        // install_default (the binary installs the ring provider at startup).
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let verifier =
            WebPkiClientVerifier::builder_with_provider(Arc::new(roots(TRUSTED_CA)), provider)
                .build()
                .expect("build client verifier");
        let now = UnixTime::now();

        // A cert signed by the trusted CA verifies.
        let good = der(CLIENT_SCALER);
        assert!(verifier.verify_client_cert(&good, &[], now).is_ok());

        // A cert signed by a DIFFERENT CA is rejected (unknown issuer).
        let bad = der(CLIENT_WRONG_CA);
        assert!(verifier.verify_client_cert(&bad, &[], now).is_err());
    }

    // --- peer-cert name extraction (CN + DNS SAN) --------------------------

    #[test]
    fn cert_names_extracts_cn_and_dns_sans() {
        let names = cert_names(der(CLIENT_SCALER).as_ref());
        assert!(
            names.contains(&"agentctl-scaler".to_string()),
            "CN: {names:?}"
        );
        assert!(
            names.contains(&"agentctl-scaler.agentctl-system.svc".to_string()),
            "SAN: {names:?}"
        );
    }

    #[test]
    fn cert_names_empty_on_garbage() {
        assert!(cert_names(b"not a cert").is_empty());
    }

    // --- allow-list in/out + empty fail-closed -----------------------------

    #[test]
    fn allowed_names_in_list_accept_not_in_list_reject() {
        let names = cert_names(der(CLIENT_SCALER).as_ref());
        // In-list (matches CN) → accepted.
        assert!(name_allowed(&names, &["agentctl-scaler".to_string()]));
        // In-list via SAN → accepted.
        assert!(name_allowed(
            &names,
            &["agentctl-scaler.agentctl-system.svc".to_string()]
        ));
        // Case-insensitive.
        assert!(name_allowed(&names, &["AGENTCTL-SCALER".to_string()]));
        // Not-in-list → rejected (the middleware turns this into 403).
        assert!(!name_allowed(&names, &["agentctl-operator".to_string()]));
        // Empty allow-list fails closed.
        assert!(!name_allowed(&names, &[]));
    }

    #[test]
    fn empty_allow_list_fails_closed_even_with_names() {
        let names = vec!["agentctl-scaler".to_string()];
        assert!(!name_allowed(&names, &[]));
    }

    // --- env parsing -------------------------------------------------------

    #[test]
    fn parse_allowed_names_trims_and_drops_empties() {
        assert_eq!(parse_allowed_names(""), Vec::<String>::new());
        assert_eq!(
            parse_allowed_names(" agentctl-scaler , agentctl-operator ,, "),
            vec![
                "agentctl-scaler".to_string(),
                "agentctl-operator".to_string()
            ]
        );
    }
}
