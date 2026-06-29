// SPDX-License-Identifier: BUSL-1.1
//! Leader election over a `coordination.k8s.io/v1` Lease (operator HA).
//!
//! The operator is a level-triggered singleton: running two reconcile loops at
//! once would race server-side applies and status patches. To stay safe at
//! `replicas > 1` (rolling upgrades, accidental scale-up), every replica
//! contends for one [`Lease`] named [`LEASE_NAME`] in the operator namespace;
//! only the holder runs the controllers. Non-holders stand by — they keep
//! serving `/healthz`, report `/readyz` 503, and poll to take over once the
//! holder's lease expires.
//!
//! This is the same acquire / renew / lose protocol `client-go`'s
//! `leaderelection` and `controller-runtime` implement, hand-rolled on the kube
//! client so we pull no extra (and potentially kube-version-skewed) dependency:
//!
//! * **acquire** — create the Lease if absent, or take it over if the current
//!   holder's `renewTime + leaseDurationSeconds` is in the past (expired);
//! * **renew** — while we hold it, rewrite `renewTime` every [`RENEW_PERIOD`];
//! * **lose** — if we cannot renew within the lease duration (apiserver
//!   unreachable) or another replica takes over, we stop being leader.
//!
//! Optimistic concurrency (the fetched `resourceVersion` carried into
//! `replace`) makes the take-over race safe: at most one replica wins each round
//! and the loser sees an HTTP 409 and backs off.

use std::sync::Arc;
use std::time::{Duration, Instant};

use k8s_openapi::api::coordination::v1::{Lease, LeaseSpec};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{MicroTime, ObjectMeta};
use kube::api::{Api, PostParams};
use tracing::{error, info, warn};

use crate::metrics::Metrics;

/// The Lease object every operator replica contends for.
pub const LEASE_NAME: &str = "agentctl-operator";

/// How long a held lease stays valid without a renewal. A standby will not take
/// over until `renewTime + LEASE_DURATION` has passed.
pub const LEASE_DURATION: Duration = Duration::from_secs(15);
/// How often the leader rewrites `renewTime` (comfortably inside the duration).
pub const RENEW_PERIOD: Duration = Duration::from_secs(10);
/// How often a standby retries to acquire, and the leader retries after a failed
/// renew.
pub const RETRY_PERIOD: Duration = Duration::from_secs(2);

/// Tunable lease timings (defaults match the [`LEASE_DURATION`] / [`RENEW_PERIOD`]
/// / [`RETRY_PERIOD`] constants).
#[derive(Clone, Copy, Debug)]
pub struct LeaseConfig {
    pub lease_duration: Duration,
    pub renew_period: Duration,
    pub retry_period: Duration,
}

impl Default for LeaseConfig {
    fn default() -> Self {
        Self {
            lease_duration: LEASE_DURATION,
            renew_period: RENEW_PERIOD,
            retry_period: RETRY_PERIOD,
        }
    }
}

/// What to do with the live Lease given who holds it and whether it has expired.
#[derive(Debug, PartialEq, Eq)]
enum Decision {
    /// We already hold it — just refresh `renewTime`.
    Renew,
    /// It is free (no holder) or expired — take it over.
    Take,
    /// Held by a live peer — stand by.
    Hold,
}

/// Pure leadership decision: given the current `holder`, its `renew_time_secs`
/// (unix seconds, if any), the `lease_duration_secs`, our `identity`, and the
/// current time `now_secs`, decide whether we may renew, take, or must stand by.
/// A held lease is considered expired once `renewTime + leaseDuration < now`.
fn decide(
    holder: Option<&str>,
    renew_time_secs: Option<i64>,
    lease_duration_secs: i64,
    identity: &str,
    now_secs: i64,
) -> Decision {
    match holder {
        Some(h) if h == identity => Decision::Renew,
        None => Decision::Take,
        Some(_) => {
            let expired = match renew_time_secs {
                Some(rt) => now_secs > rt + lease_duration_secs,
                None => true,
            };
            if expired {
                Decision::Take
            } else {
                Decision::Hold
            }
        }
    }
}

/// The current instant as a `MicroTime` (the type Lease timestamps use).
fn now_micro_time() -> MicroTime {
    MicroTime(k8s_openapi::jiff::Timestamp::now())
}

/// Build a fresh Lease object owned by `identity` (the create path).
fn new_lease(identity: &str, lease_duration_secs: i32, now: &MicroTime) -> Lease {
    Lease {
        metadata: ObjectMeta {
            name: Some(LEASE_NAME.to_string()),
            ..Default::default()
        },
        spec: Some(LeaseSpec {
            holder_identity: Some(identity.to_string()),
            acquire_time: Some(now.clone()),
            renew_time: Some(now.clone()),
            lease_duration_seconds: Some(lease_duration_secs),
            lease_transitions: Some(0),
            ..Default::default()
        }),
    }
}

/// Whether a kube error is an HTTP 409 (conflict / already-exists) — a lost race
/// for the lease that the caller should treat as "not leader this round".
fn is_conflict(err: &kube::Error) -> bool {
    matches!(err, kube::Error::Api(status) if status.code == 409)
}

/// Try once to acquire or renew the lease. Returns `Ok(true)` if we hold it after
/// this attempt, `Ok(false)` if a peer holds it (or won a concurrent race), and
/// `Err` only on an unexpected apiserver error.
async fn acquire_or_renew(
    api: &Api<Lease>,
    identity: &str,
    lease_duration: Duration,
) -> Result<bool, kube::Error> {
    let now = now_micro_time();
    let now_secs = now.0.as_second();
    let lds = lease_duration.as_secs() as i32;
    let pp = PostParams::default();

    match api.get_opt(LEASE_NAME).await? {
        // No lease yet — create one owned by us. A peer creating it first surfaces
        // as 409 → we are not leader this round.
        None => match api.create(&pp, &new_lease(identity, lds, &now)).await {
            Ok(_) => Ok(true),
            Err(e) if is_conflict(&e) => Ok(false),
            Err(e) => Err(e),
        },
        Some(mut lease) => {
            let spec = lease.spec.clone().unwrap_or_default();
            let holder = spec.holder_identity.as_deref();
            let renew_secs = spec.renew_time.as_ref().map(|t| t.0.as_second());
            match decide(holder, renew_secs, lds as i64, identity, now_secs) {
                Decision::Hold => Ok(false),
                Decision::Renew => {
                    let mut s = spec;
                    s.renew_time = Some(now.clone());
                    s.lease_duration_seconds = Some(lds);
                    lease.spec = Some(s);
                    replace_lease(api, &pp, lease).await
                }
                Decision::Take => {
                    let transitions = spec.lease_transitions.unwrap_or(0) + 1;
                    lease.spec = Some(LeaseSpec {
                        holder_identity: Some(identity.to_string()),
                        acquire_time: Some(now.clone()),
                        renew_time: Some(now.clone()),
                        lease_duration_seconds: Some(lds),
                        lease_transitions: Some(transitions),
                        ..Default::default()
                    });
                    replace_lease(api, &pp, lease).await
                }
            }
        }
    }
}

/// `replace` the lease (carrying its fetched `resourceVersion`, so a concurrent
/// writer loses with 409 → `Ok(false)`).
async fn replace_lease(
    api: &Api<Lease>,
    pp: &PostParams,
    lease: Lease,
) -> Result<bool, kube::Error> {
    match api.replace(LEASE_NAME, pp, &lease).await {
        Ok(_) => Ok(true),
        Err(e) if is_conflict(&e) => Ok(false),
        Err(e) => Err(e),
    }
}

/// Contend for leadership until won, then keep renewing in a background task.
///
/// Blocks (polling every `retry_period`) until this replica becomes leader, sets
/// the leader gauge, and spawns a renewer that rewrites `renewTime` every
/// `renew_period`. If the renewer cannot renew within `lease_duration` (apiserver
/// unreachable) or a peer takes over, it clears the gauge and **exits the
/// process** — the standard controller-runtime behaviour: terminating guarantees
/// we never run two reconcile loops, and Kubernetes restarts the pod to rejoin
/// the election once the lease frees up.
pub async fn run(api: Api<Lease>, identity: &str, cfg: LeaseConfig, metrics: Arc<Metrics>) {
    info!(identity, lease = LEASE_NAME, "contending for leadership");
    // Block until acquired (standby loop).
    loop {
        match acquire_or_renew(&api, identity, cfg.lease_duration).await {
            Ok(true) => break,
            Ok(false) => {
                metrics.set_leader(false);
            }
            Err(e) => warn!(error = %e, "lease acquire failed; retrying"),
        }
        tokio::time::sleep(cfg.retry_period).await;
    }
    metrics.set_leader(true);
    info!(identity, "acquired leadership");

    // Renew in the background; on loss, exit so no two replicas ever lead.
    let identity = identity.to_string();
    tokio::spawn(async move {
        let mut last_renew = Instant::now();
        loop {
            tokio::time::sleep(cfg.renew_period).await;
            match acquire_or_renew(&api, &identity, cfg.lease_duration).await {
                Ok(true) => last_renew = Instant::now(),
                Ok(false) => {
                    metrics.set_leader(false);
                    error!(identity, "lost leadership (lease taken over); exiting");
                    std::process::exit(1);
                }
                Err(e) => {
                    warn!(error = %e, "lease renew failed");
                    // Give up only once we can no longer guarantee we hold it.
                    if last_renew.elapsed() >= cfg.lease_duration {
                        metrics.set_leader(false);
                        error!(identity, "could not renew within lease duration; exiting");
                        std::process::exit(1);
                    }
                    tokio::time::sleep(cfg.retry_period).await;
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renews_when_we_are_the_holder() {
        assert_eq!(
            decide(Some("me"), Some(1_000), 15, "me", 1_001),
            Decision::Renew
        );
    }

    #[test]
    fn takes_a_free_lease() {
        assert_eq!(decide(None, None, 15, "me", 1_000), Decision::Take);
    }

    #[test]
    fn holds_while_peer_lease_is_live() {
        // peer renewed at t=1000, duration 15 → valid through 1015; now 1010.
        assert_eq!(
            decide(Some("peer"), Some(1_000), 15, "me", 1_010),
            Decision::Hold
        );
    }

    #[test]
    fn takes_over_an_expired_peer_lease() {
        // peer renewed at t=1000, duration 15 → expired after 1015; now 1016.
        assert_eq!(
            decide(Some("peer"), Some(1_000), 15, "me", 1_016),
            Decision::Take
        );
    }

    #[test]
    fn takes_over_a_peer_lease_with_no_renew_time() {
        assert_eq!(decide(Some("peer"), None, 15, "me", 1_000), Decision::Take);
    }

    #[test]
    fn boundary_is_strict_greater_than() {
        // exactly at renewTime + duration is NOT yet expired (matches client-go).
        assert_eq!(
            decide(Some("peer"), Some(1_000), 15, "me", 1_015),
            Decision::Hold
        );
    }

    #[test]
    fn new_lease_is_owned_by_identity() {
        let now = now_micro_time();
        let lease = new_lease("me", 15, &now);
        let spec = lease.spec.unwrap();
        assert_eq!(spec.holder_identity.as_deref(), Some("me"));
        assert_eq!(spec.lease_duration_seconds, Some(15));
        assert_eq!(spec.lease_transitions, Some(0));
        assert!(spec.acquire_time.is_some() && spec.renew_time.is_some());
        assert_eq!(lease.metadata.name.as_deref(), Some(LEASE_NAME));
    }

    #[test]
    fn default_config_matches_constants() {
        let c = LeaseConfig::default();
        assert_eq!(c.lease_duration, LEASE_DURATION);
        assert_eq!(c.renew_period, RENEW_PERIOD);
        assert_eq!(c.retry_period, RETRY_PERIOD);
        // renew comfortably inside the duration; retry is the tightest.
        assert!(c.renew_period < c.lease_duration);
        assert!(c.retry_period < c.renew_period);
    }
}
