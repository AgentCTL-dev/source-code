//! mTLS client for the gateway → node-agent hop (RFC 0015).
//!
//! The node-agent's control API now requires a CA-signed **client** certificate,
//! so only the control plane (the apiserver, this gateway) can drive the
//! `/v1/agents/...` capabilities/A2A/stream verbs. We present our client identity
//! from `/etc/agentctl-client/tls/{tls.crt,tls.key}` and verify the node-agent's
//! server cert against `/etc/agentctl-client/tls/ca.crt`.
//!
//! We deliberately DO NOT check the server hostname: the node-agent is addressed
//! by its dynamic pod IP (never present in the cert SANs). mTLS still
//! authenticates the server — only a peer holding the private key for a cert
//! chaining to our CA can complete the handshake — so skipping the name check is
//! acceptable here (the CA is the trust anchor, not DNS). See
//! [`CaServerVerifier`].
//!
//! Built on rustls 0.23 with the **ring** provider (this environment has no C
//! toolchain → never aws-lc-rs). reqwest enables this via the
//! `rustls-tls-manual-roots-no-provider` feature (no default crypto provider is
//! pulled), and we hand it a fully-built ring `ClientConfig` through
//! `use_preconfigured_tls`. The SSE passthrough (`message/stream`) still works:
//! `bytes_stream()` flows over the https connection unchanged.

use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::client::verify_server_cert_signed_by_trust_anchor;
use rustls::crypto::{
    ring, verify_tls12_signature, verify_tls13_signature, WebPkiSupportedAlgorithms,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::server::ParsedCertificate;
use rustls::{DigitallySignedStruct, RootCertStore, SignatureScheme};

/// Where the gateway's mounted **client** identity + trust anchor live.
const CLIENT_TLS_DIR: &str = "/etc/agentctl-client/tls";

/// A `ServerCertVerifier` that verifies the node-agent's cert chains to our CA
/// but does NOT check the server name (the node-agent is reached by dynamic pod
/// IP). Signature verification reuses ring's algorithms.
#[derive(Debug)]
struct CaServerVerifier {
    roots: RootCertStore,
    supported: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for CaServerVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let cert = ParsedCertificate::try_from(end_entity)?;
        // Chain-to-CA only — intentionally skip `verify_server_name`: the
        // node-agent has no stable DNS name, only a churning pod IP. mTLS + this
        // CA still prove the peer's identity.
        verify_server_cert_signed_by_trust_anchor(
            &cert,
            &self.roots,
            intermediates,
            now,
            self.supported.all,
        )?;
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(message, cert, dss, &self.supported)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.supported)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.supported.supported_schemes()
    }
}

/// Build the mTLS reqwest client for the node-agent hop. Built once and shared
/// (it's an `Arc` internally). Panics at startup if the mounted cert material is
/// missing or malformed — there is no safe degraded mode for the control hop.
pub fn node_agent_client() -> reqwest::Client {
    try_build().expect("build node-agent mTLS client")
}

fn try_build() -> Result<reqwest::Client, String> {
    let dir = PathBuf::from(CLIENT_TLS_DIR);
    let certs = load_certs(&dir.join("tls.crt"))?;
    let key = load_key(&dir.join("tls.key"))?;
    let roots = load_roots(&dir.join("ca.crt"))?;

    // Explicit ring provider — never the (absent) process default, so the
    // gateway need not install one for its own ClientConfig.
    let provider = ring::default_provider();
    let supported = provider.signature_verification_algorithms;
    let verifier = Arc::new(CaServerVerifier { roots, supported });

    let config = rustls::ClientConfig::builder_with_provider(Arc::new(provider))
        .with_safe_default_protocol_versions()
        .map_err(|e| format!("tls protocol versions: {e}"))?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_client_auth_cert(certs, key)
        .map_err(|e| format!("client auth cert: {e}"))?;

    reqwest::Client::builder()
        .use_preconfigured_tls(config)
        .build()
        .map_err(|e| format!("build reqwest client: {e}"))
}

fn load_certs(path: &Path) -> Result<Vec<CertificateDer<'static>>, String> {
    let mut r =
        BufReader::new(std::fs::File::open(path).map_err(|e| format!("open {path:?}: {e}"))?);
    rustls_pemfile::certs(&mut r)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("read client certs {path:?}: {e}"))
}

fn load_key(path: &Path) -> Result<PrivateKeyDer<'static>, String> {
    let mut r =
        BufReader::new(std::fs::File::open(path).map_err(|e| format!("open {path:?}: {e}"))?);
    rustls_pemfile::private_key(&mut r)
        .map_err(|e| format!("read client key {path:?}: {e}"))?
        .ok_or_else(|| format!("no private key in {path:?}"))
}

fn load_roots(path: &Path) -> Result<RootCertStore, String> {
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
