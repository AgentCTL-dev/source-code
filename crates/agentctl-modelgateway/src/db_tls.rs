// SPDX-License-Identifier: BUSL-1.1
//! TLS connector for the Postgres hop (DSN `sslmode=require`).
//!
//! Default deployments use `sslmode=disable` → [`tokio_postgres::NoTls`], the
//! pure pass-through (see `build_pool` in `main.rs`). When the bundled Postgres is
//! deployed with TLS (`postgres.bundled.tls.enabled`, DATABASE_URL
//! `sslmode=require`) the store wraps the connection in rustls so the hop is
//! encrypted.
//!
//! `sslmode=require` means *encrypt, but do NOT verify the server certificate*
//! (libpq semantics — distinct from `verify-ca`/`verify-full`). We therefore
//! install a [`NoVerify`] certificate verifier: it still checks the handshake
//! signature against the presented end-entity key (so this is a real TLS channel
//! to whoever holds that key), but it does not validate the chain or the server
//! name. The control plane reaches the bundled Postgres over a NetworkPolicy-
//! scoped in-cluster Service.
//!
//! **`verify-full` (CA pinning).** When the DSN asks for `sslmode=verify-full`
//! (libpq semantics: verify the chain to a trusted CA *and* match the server
//! name) — or the operator sets `DB_TLS_VERIFY=full` — and a CA bundle is mounted
//! (env `PGSSLROOTCERT` or `DB_CA_FILE`, default [`DEFAULT_CA_FILE`]), we install
//! a [`rustls::client::WebPkiServerVerifier`] seeded from that CA via
//! [`make_verifying_connector`]. tokio-postgres 0.7 cannot itself parse
//! `sslmode=verify-full`, so [`resolve_tls`] rewrites it down to `require` (which
//! keeps the hop encrypted) and reports the verify intent out-of-band. If the CA
//! cannot be loaded we fall back to the encrypt-without-verify behaviour (the
//! caller logs a warning) so a missing mount never silently drops to plaintext.
//!
//! Built on rustls 0.23 with the **ring** provider (no aws-lc-rs, no C toolchain
//! — matches the rest of the agentctl stack).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::client::WebPkiServerVerifier;
use rustls::crypto::{
    ring, verify_tls12_signature, verify_tls13_signature, WebPkiSupportedAlgorithms,
};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, RootCertStore, SignatureScheme};
use tokio_postgres_rustls::MakeRustlsConnect;

/// Default mount path for the agentctl Postgres CA bundle (overridable via
/// `PGSSLROOTCERT` or `DB_CA_FILE`).
pub const DEFAULT_CA_FILE: &str = "/etc/agentctl-pg-ca/ca.crt";

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

/// Resolve the CA bundle path for `verify-full`: `PGSSLROOTCERT` (libpq-compatible)
/// or `DB_CA_FILE`, falling back to [`DEFAULT_CA_FILE`].
pub fn ca_file_path() -> PathBuf {
    std::env::var_os("PGSSLROOTCERT")
        .or_else(|| std::env::var_os("DB_CA_FILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_CA_FILE))
}

/// Normalise the DSN for `verify-full` and report whether CA verification was
/// requested.
///
/// tokio-postgres 0.7 only parses `disable`/`prefer`/`require`, so a libpq
/// `sslmode=verify-full` (or `verify-ca`) would fail to parse. We rewrite it down
/// to `sslmode=require` — the hop stays encrypted — and return `true` so the
/// caller installs the CA-pinning verifier. `DB_TLS_VERIFY=full` requests the same
/// out-of-band (e.g. for a plain `sslmode=require` DSN).
pub fn resolve_tls(raw_dsn: &str) -> (String, bool) {
    let mut verify = std::env::var("DB_TLS_VERIFY")
        .map(|v| v.eq_ignore_ascii_case("full"))
        .unwrap_or(false);
    let mut dsn = raw_dsn.to_string();
    for needle in ["sslmode=verify-full", "sslmode=verify-ca"] {
        if let Some(pos) = dsn.find(needle) {
            dsn.replace_range(pos..pos + needle.len(), "sslmode=require");
            verify = true;
        }
    }
    (dsn, verify)
}

/// Build a CA-pinning (`verify-full`) rustls connector for the Postgres hop: the
/// server cert chain is verified against the CA bundle at `ca_path` *and* the
/// server name is matched (full libpq `verify-full` semantics). Uses an explicit
/// ring provider + [`WebPkiServerVerifier`] (no process-default crypto provider).
///
/// Returns `Err` when the CA file is missing/unreadable or contains no
/// certificates, so the caller can fall back to encrypt-without-verify.
pub fn make_verifying_connector(
    ca_path: &Path,
) -> Result<MakeRustlsConnect, Box<dyn std::error::Error + Send + Sync>> {
    let pem = std::fs::read(ca_path)?;
    let mut roots = RootCertStore::empty();
    for cert in rustls_pemfile::certs(&mut pem.as_slice()) {
        roots.add(cert?)?;
    }
    if roots.is_empty() {
        return Err(format!("no CA certificates found in {}", ca_path.display()).into());
    }
    let provider = Arc::new(ring::default_provider());
    let verifier =
        WebPkiServerVerifier::builder_with_provider(Arc::new(roots), provider.clone()).build()?;
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("tls protocol versions")
        .with_webpki_verifier(verifier)
        .with_no_client_auth();
    Ok(MakeRustlsConnect::new(config))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_CA_PEM: &[u8] = include_bytes!("../testdata/pg_ca.crt");

    #[test]
    fn resolve_tls_rewrites_verify_full_to_require() {
        let (dsn, verify) =
            resolve_tls("postgres://u:p@h:5432/db?sslmode=verify-full&connect_timeout=5");
        assert_eq!(
            dsn,
            "postgres://u:p@h:5432/db?sslmode=require&connect_timeout=5"
        );
        assert!(verify);
    }

    #[test]
    fn resolve_tls_rewrites_verify_ca_to_require() {
        let (dsn, verify) = resolve_tls("host=h sslmode=verify-ca dbname=db");
        assert_eq!(dsn, "host=h sslmode=require dbname=db");
        assert!(verify);
    }

    #[test]
    fn resolve_tls_leaves_require_untouched() {
        // Without DB_TLS_VERIFY set, a plain require DSN is not a verify request.
        if std::env::var_os("DB_TLS_VERIFY").is_none() {
            let (dsn, verify) = resolve_tls("postgres://u:p@h/db?sslmode=require");
            assert_eq!(dsn, "postgres://u:p@h/db?sslmode=require");
            assert!(!verify);
        }
    }

    #[test]
    fn make_verifying_connector_loads_a_valid_ca() {
        let dir = std::env::temp_dir().join(format!("agentctl-pg-ca-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ca.crt");
        std::fs::write(&path, TEST_CA_PEM).unwrap();
        assert!(make_verifying_connector(&path).is_ok());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn make_verifying_connector_errors_on_missing_file() {
        let path = Path::new("/nonexistent/agentctl-pg-ca/ca.crt");
        assert!(make_verifying_connector(path).is_err());
    }

    #[test]
    fn make_verifying_connector_errors_on_empty_bundle() {
        let dir = std::env::temp_dir().join(format!("agentctl-pg-ca-empty-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("empty.crt");
        std::fs::write(&path, b"not a certificate\n").unwrap();
        assert!(make_verifying_connector(&path).is_err());
        let _ = std::fs::remove_file(&path);
    }
}
