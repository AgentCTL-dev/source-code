// SPDX-License-Identifier: BUSL-1.1
//! Coordination HTTP client construction + the mTLS-vs-plaintext decision.
//!
//! The scaler reads the coordination server's off-pod backlog (`work.stats`) over
//! HTTP. How it authenticates that hop is gated on whether a client-cert directory
//! is configured:
//!
//!   * **`COORDINATION_CLIENT_CERT_DIR` UNSET/empty (default)** — a plain
//!     `reqwest::Client` over plaintext http, with the optional `AGENTCTL_API_TOKEN`
//!     bearer applied per-request by [`crate::scaler`].
//!   * **`COORDINATION_CLIENT_CERT_DIR` set** — the scaler presents a CLIENT
//!     certificate (mTLS). It loads `<dir>/tls.crt` + `<dir>/tls.key` as its
//!     identity and verifies the coordination SERVER cert against the CA in
//!     `COORDINATION_CA` (defaulting to `<dir>/ca.crt`). The chart points
//!     `coordinationUrl` at the coordination server's https `:8443` listener. The
//!     bearer may still be presented if set (harmless); mTLS is the stronger auth.
//!
//! Built on rustls 0.23 with the **ring** provider (this environment has no C
//! toolchain → never aws-lc-rs). reqwest enables this via the
//! `rustls-tls-manual-roots-no-provider` feature (no default crypto provider is
//! pulled); we hand it a fully-built ring `ClientConfig` through
//! `use_preconfigured_tls`. Server-name verification is left ON: the coordination
//! server has a stable in-cluster service DNS name, so the standard webpki verifier
//! against the configured CA is the right, safer choice.

use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rustls::crypto::ring;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::RootCertStore;

/// Env: directory holding the scaler's client identity (`tls.crt`, `tls.key`),
/// e.g. `/etc/agentctl-scaler-mtls`. UNSET/empty ⇒ plaintext.
pub const ENV_CLIENT_CERT_DIR: &str = "COORDINATION_CLIENT_CERT_DIR";
/// Env: CA file used to verify the coordination SERVER cert. Defaults to
/// `<cert_dir>/ca.crt` when unset (only consulted when `ENV_CLIENT_CERT_DIR` set).
pub const ENV_CA: &str = "COORDINATION_CA";

/// Filenames under the client-cert dir (the standard Kubernetes TLS secret layout).
const CERT_FILE: &str = "tls.crt";
const KEY_FILE: &str = "tls.key";
const DEFAULT_CA_FILE: &str = "ca.crt";

/// The selected coordination-hop transport: plaintext (default) or mTLS with a
/// resolved set of PEM paths. Pure data — the path decision is unit-testable
/// without touching the filesystem or a live server (see [`decide_client_mode`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientMode {
    /// Plaintext http + optional `AGENTCTL_API_TOKEN` bearer (cert dir unset).
    Plaintext,
    /// Present a client cert (mTLS) and verify the server cert against `ca`.
    Mtls {
        /// `<cert_dir>/tls.crt` — the client identity's certificate chain.
        cert: PathBuf,
        /// `<cert_dir>/tls.key` — the client identity's private key.
        key: PathBuf,
        /// CA to verify the coordination SERVER cert (`COORDINATION_CA` or
        /// `<cert_dir>/ca.crt`).
        ca: PathBuf,
    },
}

/// Decide the transport from the two env values (already extracted, so this is a
/// pure function). `cert_dir` UNSET/empty/whitespace ⇒ [`ClientMode::Plaintext`];
/// otherwise mTLS, with the CA defaulting to `<cert_dir>/ca.crt` when `ca` is
/// unset/empty.
pub fn decide_client_mode(cert_dir: Option<&str>, ca: Option<&str>) -> ClientMode {
    let Some(dir) = cert_dir.map(str::trim).filter(|s| !s.is_empty()) else {
        return ClientMode::Plaintext;
    };
    let dir = Path::new(dir);
    let ca = ca
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| dir.join(DEFAULT_CA_FILE));
    ClientMode::Mtls {
        cert: dir.join(CERT_FILE),
        key: dir.join(KEY_FILE),
        ca,
    }
}

/// Read the two env vars and decide the transport (the runtime wrapper around
/// [`decide_client_mode`]).
pub fn mode_from_env() -> ClientMode {
    let cert_dir = std::env::var(ENV_CLIENT_CERT_DIR).ok();
    let ca = std::env::var(ENV_CA).ok();
    decide_client_mode(cert_dir.as_deref(), ca.as_deref())
}

/// Build the coordination HTTP client for the selected mode. For
/// [`ClientMode::Plaintext`] this is a bare client (matches today). For
/// [`ClientMode::Mtls`] it loads the mounted PEMs and builds a ring-backed
/// `ClientConfig`; a missing/malformed cert dir is a hard error (there is no safe
/// degraded mode once mTLS is requested).
pub fn build_client(mode: &ClientMode) -> Result<reqwest::Client, String> {
    match mode {
        ClientMode::Plaintext => reqwest::Client::builder()
            .build()
            .map_err(|e| format!("build plaintext reqwest client: {e}")),
        ClientMode::Mtls { cert, key, ca } => build_mtls_client(cert, key, ca),
    }
}

fn build_mtls_client(cert: &Path, key: &Path, ca: &Path) -> Result<reqwest::Client, String> {
    let certs = load_certs(cert)?;
    let key = load_key(key)?;
    let roots = load_roots(ca)?;

    // Explicit ring provider — never the (absent) process default, so the scaler
    // need not install one for its own ClientConfig. Server-name verification ON:
    // `with_root_certificates` builds the standard webpki verifier against `ca`.
    let provider = ring::default_provider();
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(provider))
        .with_safe_default_protocol_versions()
        .map_err(|e| format!("tls protocol versions: {e}"))?
        .with_root_certificates(roots)
        .with_client_auth_cert(certs, key)
        .map_err(|e| format!("client auth cert: {e}"))?;

    reqwest::Client::builder()
        .use_preconfigured_tls(config)
        .build()
        .map_err(|e| format!("build mTLS reqwest client: {e}"))
}

fn load_certs(path: &Path) -> Result<Vec<CertificateDer<'static>>, String> {
    let mut r =
        BufReader::new(std::fs::File::open(path).map_err(|e| format!("open {path:?}: {e}"))?);
    let certs = rustls_pemfile::certs(&mut r)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("read client certs {path:?}: {e}"))?;
    if certs.is_empty() {
        return Err(format!("client cert file {path:?} had no certs"));
    }
    Ok(certs)
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

#[cfg(test)]
mod tests {
    use super::*;

    // --- the transport DECISION (mTLS iff the cert dir is set) ------------------

    #[test]
    fn unset_cert_dir_is_plaintext() {
        assert_eq!(decide_client_mode(None, None), ClientMode::Plaintext);
        // An explicit CA without a cert dir is still plaintext — mTLS needs the
        // client identity, which lives in the cert dir.
        assert_eq!(
            decide_client_mode(None, Some("/etc/ca/ca.crt")),
            ClientMode::Plaintext
        );
    }

    #[test]
    fn empty_or_whitespace_cert_dir_is_plaintext() {
        assert_eq!(decide_client_mode(Some(""), None), ClientMode::Plaintext);
        assert_eq!(decide_client_mode(Some("   "), None), ClientMode::Plaintext);
    }

    #[test]
    fn cert_dir_set_selects_mtls_with_default_ca() {
        // CA unset ⇒ <cert_dir>/ca.crt; identity from <cert_dir>/{tls.crt,tls.key}.
        let mode = decide_client_mode(Some("/etc/agentctl-scaler-mtls"), None);
        assert_eq!(
            mode,
            ClientMode::Mtls {
                cert: PathBuf::from("/etc/agentctl-scaler-mtls/tls.crt"),
                key: PathBuf::from("/etc/agentctl-scaler-mtls/tls.key"),
                ca: PathBuf::from("/etc/agentctl-scaler-mtls/ca.crt"),
            }
        );
    }

    #[test]
    fn explicit_ca_overrides_the_default() {
        let mode = decide_client_mode(
            Some("/etc/agentctl-scaler-mtls"),
            Some("/etc/trust/coordination-ca.crt"),
        );
        match mode {
            ClientMode::Mtls { ca, cert, key } => {
                assert_eq!(ca, PathBuf::from("/etc/trust/coordination-ca.crt"));
                assert_eq!(cert, PathBuf::from("/etc/agentctl-scaler-mtls/tls.crt"));
                assert_eq!(key, PathBuf::from("/etc/agentctl-scaler-mtls/tls.key"));
            }
            ClientMode::Plaintext => panic!("expected mTLS"),
        }
        // A blank CA falls back to the default, same as unset.
        assert_eq!(
            decide_client_mode(Some("/d"), Some("  ")),
            ClientMode::Mtls {
                cert: PathBuf::from("/d/tls.crt"),
                key: PathBuf::from("/d/tls.key"),
                ca: PathBuf::from("/d/ca.crt"),
            }
        );
    }

    // --- the BUILD path (no live server needed) --------------------------------

    #[test]
    fn build_plaintext_client_succeeds() {
        // reqwest's rustls backend resolves the process-default provider when
        // building any client; install ring (idempotent — main() does the same).
        let _ = ring::default_provider().install_default();
        // The default (plaintext) path always builds.
        assert!(build_client(&ClientMode::Plaintext).is_ok());
    }

    #[test]
    fn build_mtls_with_missing_material_errors_not_panics() {
        // mTLS requested but the mounted PEMs are absent ⇒ a clean Err (no panic,
        // no silent plaintext fallback). Exercises the build path without a server.
        let mode = decide_client_mode(Some("/nonexistent-agentctl-scaler-mtls"), None);
        let err = build_client(&mode).expect_err("missing cert material must error");
        assert!(
            err.contains("tls.crt"),
            "error names the missing cert: {err}"
        );
    }
}
