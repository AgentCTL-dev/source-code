// SPDX-License-Identifier: BUSL-1.1
//! agentctl-identity binary: config from env, custody store (PG or memory —
//! memory says so LOUDLY: custody dies with the pod), federation client, and
//! the axum surface. TLS terminates at the mesh layer in P1 (strict
//! NetworkPolicy fronting this pod); native rustls serving rides the same
//! hardening pass as the other control-plane listeners.

use std::sync::Arc;

use agentctl_identity::http::{router, AppState};
use agentctl_identity::oidc::{outbound_client, Federation};
use agentctl_identity::store::{MemoryStore, PgStore, Store};
use agentctl_identity::Config;
use tracing::{info, warn};

#[tokio::main]
async fn main() {
    agentctl_telemetry::init("agentctl-identity");
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("install ring crypto provider");

    let cfg = match Config::from_env() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("identity config error: {e}");
            std::process::exit(2);
        }
    };
    if cfg.providers.is_empty() {
        warn!("no IDENTITY_PROVIDERS configured — introspection and device login will refuse");
    }
    if cfg.seal_key.is_none() {
        warn!("IDENTITY_SEAL_KEY not set — using an EPHEMERAL seal key (grants die with this pod); wire a real key before storing connections");
    }

    let store: Arc<dyn Store> = match &cfg.store {
        agentctl_identity::config::StoreConfig::Postgres { dsn } => {
            match PgStore::connect(dsn).await {
                Ok(pg) => {
                    info!("custody store: postgres");
                    Arc::new(pg)
                }
                Err(e) => {
                    eprintln!("identity store error: {e}");
                    std::process::exit(1);
                }
            }
        }
        agentctl_identity::config::StoreConfig::Memory => {
            warn!("custody store: MEMORY — sessions/principals die with this pod (dev only)");
            Arc::new(MemoryStore::default())
        }
    };

    let outbound = match &cfg.issuer_ca {
        Some(path) => {
            let pem = match std::fs::read(path) {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("identity config error: IDENTITY_ISSUER_CA {path}: {e}");
                    std::process::exit(2);
                }
            };
            match agentctl_identity::oidc::outbound_client_with_extra_roots(&pem) {
                Ok(c) => {
                    info!(path, "issuer TLS: webpki + extra private-CA roots");
                    c
                }
                Err(e) => {
                    eprintln!("identity config error: {e}");
                    std::process::exit(2);
                }
            }
        }
        None => outbound_client(),
    };
    let state = Arc::new(AppState {
        federation: Federation::new(outbound, cfg.providers.clone()),
        store,
        admin_token: cfg.admin_token.clone(),
    });

    info!(bind = %cfg.bind, providers = cfg.providers.len(), "agentctl-identity serving");
    let listener = tokio::net::TcpListener::bind(cfg.bind)
        .await
        .expect("bind identity listener");
    axum::serve(listener, router(state))
        .with_graceful_shutdown(shutdown())
        .await
        .expect("identity server");
}

async fn shutdown() {
    let ctrl_c = async { tokio::signal::ctrl_c().await.ok() };
    #[cfg(unix)]
    let term = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("sigterm handler")
            .recv()
            .await
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<Option<()>>();
    tokio::select! { _ = ctrl_c => {}, _ = term => {} }
}
