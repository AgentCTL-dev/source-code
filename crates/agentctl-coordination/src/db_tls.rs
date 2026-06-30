// SPDX-License-Identifier: BUSL-1.1
//! TLS connector for the Postgres hop (DSN `sslmode=require`).
//!
//! Default deployments use `sslmode=disable` → [`tokio_postgres::NoTls`], the
//! pure pass-through (see `build_pool` in `pg_store.rs`). When the bundled
//! Postgres is deployed with TLS (`postgres.bundled.tls.enabled`,
//! `COORDINATION_DATABASE_URL`/`DATABASE_URL` with `sslmode=require`) the store
//! wraps the connection in rustls so the hop is encrypted.
//!
//! `sslmode=require` means *encrypt, but do NOT verify the server certificate*
//! (libpq semantics — distinct from `verify-ca`/`verify-full`). We therefore
//! install a [`NoVerify`] certificate verifier: it still checks the handshake
//! signature against the presented end-entity key (so this is a real TLS channel
//! to whoever holds that key), but it does not validate the chain or the server
//! name. The control plane reaches the bundled Postgres over a NetworkPolicy-
//! scoped in-cluster Service; CA-verifying (`verify-full`) the bundled cert is
//! future hardening — for a verified DSN today, point at an external managed
//! Postgres. Built on rustls 0.23 with the **ring** provider (no aws-lc-rs, no C
//! toolchain — the SAME stack the gateway/modelgateway use for their stores).

use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{
    ring, verify_tls12_signature, verify_tls13_signature, WebPkiSupportedAlgorithms,
};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use tokio_postgres_rustls::MakeRustlsConnect;

/// Accepts any server certificate (encrypt-without-verify, libpq `sslmode=require`).
#[derive(Debug)]
struct NoVerify {
    supported: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for NoVerify {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        // sslmode=require: encrypt the hop, do not verify the chain/name.
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

/// Build the rustls TLS connector for the Postgres hop (encrypt-without-verify).
/// Uses an explicit ring provider so no process-default crypto provider is needed.
pub fn make_connector() -> MakeRustlsConnect {
    let provider = ring::default_provider();
    let supported = provider.signature_verification_algorithms;
    let verifier = Arc::new(NoVerify { supported });
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(provider))
        .with_safe_default_protocol_versions()
        .expect("tls protocol versions")
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    MakeRustlsConnect::new(config)
}
