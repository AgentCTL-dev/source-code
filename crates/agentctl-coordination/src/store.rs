// SPDX-License-Identifier: BUSL-1.1
//! The claim store — the **single serializing point** (agentctl RFC 0011 §3.2).
//!
//! Everything that makes exactly-one-owner hold lives here, behind ONE `Mutex`:
//! the atomic `work.claim` (exactly one of N concurrent racers for an item wins),
//! the lease lifecycle (`renew`/`ack`/`release` + TTL expiry), and the
//! transactional dedupe on `claim_key` (a redelivered-but-already-acked item is a
//! no-op — the at-least-once safety net, agentd RFC 0019 §3.5).
//!
//! The store sits behind the [`ClaimStore`] trait so a durable backend
//! (Redis/Postgres) can slot in later WITHOUT touching the MCP wire layer; v1
//! ships the in-memory [`InMemoryStore`]. HA / durability of the single replica is
//! the open question recorded in agentctl RFC 0011 §3.2 / §10 — a coordination
//! loss collapses the serializing point for every fleet that depends on it.
//!
//! Time is tracked with [`Instant`] (monotonic) — never wall-clock — so a clock
//! step can never resurrect or prematurely expire a lease.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Sentinel prefix on the error returned by a verifying lifecycle op
/// (`ack`/`renew`/`release`) when the caller's attested identity is NOT the lease's
/// recorded holder — i.e. a tenant tried to settle or steal another tenant's lease.
/// Centralised so the wire layer can recognise the rejection (via
/// [`is_holder_mismatch`]) and count an attestation reject, without coupling to the
/// exact message text.
pub const HOLDER_MISMATCH_PREFIX: &str = "forbidden(holder-mismatch):";

/// The error a verifying lifecycle op returns on a holder mismatch. See
/// [`HOLDER_MISMATCH_PREFIX`].
pub fn holder_mismatch_error(lease_id: &str) -> String {
    format!("{HOLDER_MISMATCH_PREFIX} lease {lease_id} is held by a different identity")
}

/// Whether an error string is a holder-mismatch rejection (see
/// [`holder_mismatch_error`]). The wire layer uses this to count the rejection as
/// an attestation reject and surface a 403-style error to the caller.
pub fn is_holder_mismatch(err: &str) -> bool {
    err.starts_with(HOLDER_MISMATCH_PREFIX)
}

/// Outcome of a `work.claim` round-trip — the atomic grant decision (agentd RFC
/// 0019 §3.3). Exactly one of these is returned under the store mutex.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaimResult {
    /// The caller owns the item until the lease expires (or is acked/released).
    Granted {
        lease_id: String,
        expires_in_ms: u64,
    },
    /// A live lease for this item is held by another holder (`held_by` is its
    /// reported identity; `None` when unknown).
    Contended { held_by: Option<String> },
    /// The `claim_key` is already in the done set — the item was processed and
    /// acked. Never re-granted (DEDUPE; the wire reports `held_by:"<acked>"`).
    Deduped,
}

/// Outcome of `work.submit` — enqueueing into the backlog (the P9 scale-from-zero
/// signal). Only [`SubmitOutcome::Enqueued`] grows `pending`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubmitOutcome {
    /// Newly enqueued into `pending`.
    Enqueued,
    /// `claim_key` already in the done set — skipped (dedupe).
    Deduped,
    /// Already in `pending`.
    AlreadyPending,
    /// Currently held under a live lease.
    AlreadyClaimed,
}

/// A snapshot of queue depth (agentd RFC 0011 §3.2 / agentctl RFC 0011 §5.3): the
/// off-pod backlog the external scaler reads to scale from zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    /// Items enqueued and not yet claimed.
    pub pending: usize,
    /// Items held under a *live* lease (expired-but-unswept leases are excluded).
    pub claimed: usize,
    /// Age of the OLDEST pending item, in ms (0 when nothing is pending) — the
    /// backlog-staleness signal.
    pub oldest_age_ms: u64,
}

/// The pluggable coordination backend. The MCP wire layer ([`crate::mcp`]) only
/// ever sees this trait, so a durable backend (Redis/Postgres) drops in without a
/// wire change (agentctl RFC 0011 §3.2: the server is pluggable).
///
/// Every method is its own atomic transaction. Implementations MUST serialize
/// `claim` so exactly one of N concurrent racers for the same item is granted.
pub trait ClaimStore: Send + Sync {
    /// Enqueue `item` into the backlog. Skipped if `claim_key` is already done.
    fn submit(&self, item: &str, claim_key: Option<&str>) -> SubmitOutcome;
    /// Atomically claim `item` for `holder` under `claim_key`, for `ttl_ms`.
    fn claim(&self, item: &str, ttl_ms: u64, claim_key: &str, holder: &str) -> ClaimResult;
    /// Extend a live, owned lease. `Err` for an unknown or stale (expired) lease —
    /// renew NEVER resurrects an expired lease (agentd RFC 0019 §3.6).
    ///
    /// `expected_holder` is the attested-identity gate (RFC 0015): `None` ⇒ no
    /// constraint (attest mode off, back-compat); `Some(h)` ⇒ the op is allowed
    /// ONLY when the lease's recorded holder equals `h`, else it returns the
    /// [`holder_mismatch_error`] (a tenant cannot renew another tenant's lease).
    /// Implementations MUST enforce this ATOMICALLY with the mutation.
    fn renew(
        &self,
        lease_id: &str,
        ttl_ms: u64,
        expected_holder: Option<&str>,
    ) -> Result<(), String>;
    /// Settle a lease AND record its `claim_key` in the done set (terminal). The
    /// passed `claim_key` lets an already-acked item resolve idempotently.
    ///
    /// `expected_holder` gates the settle to the attested holder (see [`renew`] —
    /// `None` ⇒ unconstrained; `Some(h)` ⇒ holder must match, else
    /// [`holder_mismatch_error`]).
    ///
    /// [`renew`]: ClaimStore::renew
    fn ack(
        &self,
        lease_id: &str,
        claim_key: &str,
        expected_holder: Option<&str>,
    ) -> Result<(), String>;
    /// Return a held item to `pending` (re-claimable). `Err` for an unknown lease.
    ///
    /// `expected_holder` gates the release to the attested holder (see [`renew`] —
    /// `None` ⇒ unconstrained; `Some(h)` ⇒ holder must match, else
    /// [`holder_mismatch_error`]).
    ///
    /// [`renew`]: ClaimStore::renew
    fn release(
        &self,
        lease_id: &str,
        reason: &str,
        expected_holder: Option<&str>,
    ) -> Result<(), String>;
    /// Move every expired lease back to `pending`; returns how many were swept.
    fn sweep_expired(&self) -> usize;
    /// Current queue depth (P9 backlog snapshot).
    fn stats(&self) -> Stats;
    /// The current pending items (for the `work://pending` resource read).
    fn pending_items(&self) -> Vec<String>;
}

/// A live lease over one item.
#[derive(Debug, Clone)]
struct Lease {
    lease_id: String,
    /// Reported as `held_by` to a contending claimer.
    holder: String,
    /// The dedupe key recorded into the done set on a terminal `ack`.
    claim_key: String,
    /// Monotonic expiry. `<= now` ⇒ expired (re-claimable / sweepable).
    expires_at: Instant,
}

/// A bounded FIFO dedupe set of `claim_key`s (the "done" set).
///
/// **The bound (documented):** capped at `cap` entries; when full, the OLDEST key
/// is evicted first (insertion order). A redelivery of an item whose key was
/// evicted could be re-granted — acceptable because (a) the cap is large
/// (default 100k) so the eviction horizon is far past any realistic
/// in-flight/redelivery window, and (b) the downstream side effect is itself
/// `claim_key`-idempotent (agentd RFC 0019 §3.5), so a re-grant past the horizon
/// re-collapses to one effect. This keeps the set from growing unbounded.
#[derive(Debug)]
struct DoneSet {
    set: HashSet<String>,
    order: VecDeque<String>,
    cap: usize,
}

impl DoneSet {
    fn new(cap: usize) -> Self {
        Self {
            set: HashSet::new(),
            order: VecDeque::new(),
            cap: cap.max(1),
        }
    }

    fn contains(&self, key: &str) -> bool {
        self.set.contains(key)
    }

    fn insert(&mut self, key: String) {
        if self.set.contains(&key) {
            return; // already done; don't disturb eviction order
        }
        self.set.insert(key.clone());
        self.order.push_back(key);
        while self.set.len() > self.cap {
            match self.order.pop_front() {
                Some(old) => {
                    self.set.remove(&old);
                }
                None => break,
            }
        }
    }
}

/// Everything guarded by the single store mutex.
#[derive(Debug)]
struct Inner {
    /// Enqueued-but-unclaimed items → their enqueue instant (for `oldest_age_ms`).
    pending: HashMap<String, Instant>,
    /// item → its live lease.
    claimed: HashMap<String, Lease>,
    /// lease_id → item (reverse lookup for renew/ack/release).
    lease_index: HashMap<String, String>,
    /// The bounded dedupe set of acked `claim_key`s.
    done: DoneSet,
    /// Monotonic lease-id counter (uniqueness without wall-clock / RNG).
    seq: u64,
}

/// The v1 in-memory store: all state behind a single [`Mutex`] — THE serializing
/// point. Wrap in an `Arc` and share across the axum handlers and the sweeper.
#[derive(Debug)]
pub struct InMemoryStore {
    inner: Mutex<Inner>,
}

impl InMemoryStore {
    /// Construct with a dedupe-set capacity (`dedupe_cap`, the FIFO bound).
    pub fn new(dedupe_cap: usize) -> Self {
        Self {
            inner: Mutex::new(Inner {
                pending: HashMap::new(),
                claimed: HashMap::new(),
                lease_index: HashMap::new(),
                done: DoneSet::new(dedupe_cap),
                seq: 0,
            }),
        }
    }
}

impl ClaimStore for InMemoryStore {
    fn submit(&self, item: &str, claim_key: Option<&str>) -> SubmitOutcome {
        let mut g = self.inner.lock().expect("store mutex poisoned");
        let now = Instant::now();
        if let Some(key) = claim_key {
            if g.done.contains(key) {
                return SubmitOutcome::Deduped;
            }
        }
        if let Some(lease) = g.claimed.get(item) {
            if lease.expires_at > now {
                return SubmitOutcome::AlreadyClaimed;
            }
        }
        if g.pending.contains_key(item) {
            return SubmitOutcome::AlreadyPending;
        }
        g.pending.insert(item.to_string(), now);
        SubmitOutcome::Enqueued
    }

    fn claim(&self, item: &str, ttl_ms: u64, claim_key: &str, holder: &str) -> ClaimResult {
        let mut g = self.inner.lock().expect("store mutex poisoned");
        let now = Instant::now();

        // DEDUPE first: an already-acked key is never re-granted.
        if g.done.contains(claim_key) {
            return ClaimResult::Deduped;
        }

        // A live lease for this item ⇒ contention. (An expired lease falls
        // through and is superseded below — lazy reclaim, correct between sweeps.)
        if let Some(lease) = g.claimed.get(item) {
            if lease.expires_at > now {
                return ClaimResult::Contended {
                    held_by: Some(lease.holder.clone()),
                };
            }
        }
        if let Some(old) = g.claimed.remove(item) {
            g.lease_index.remove(&old.lease_id);
        }

        // GRANT.
        g.seq += 1;
        let seq = g.seq;
        let lease_id = make_lease_id(seq, item, holder);
        let lease = Lease {
            lease_id: lease_id.clone(),
            holder: holder.to_string(),
            claim_key: claim_key.to_string(),
            expires_at: now + Duration::from_millis(ttl_ms),
        };
        g.claimed.insert(item.to_string(), lease);
        g.lease_index.insert(lease_id.clone(), item.to_string());
        g.pending.remove(item);
        ClaimResult::Granted {
            lease_id,
            expires_in_ms: ttl_ms,
        }
    }

    fn renew(
        &self,
        lease_id: &str,
        ttl_ms: u64,
        expected_holder: Option<&str>,
    ) -> Result<(), String> {
        let mut g = self.inner.lock().expect("store mutex poisoned");
        let now = Instant::now();
        let item = g
            .lease_index
            .get(lease_id)
            .cloned()
            .ok_or_else(|| format!("unknown lease_id: {lease_id}"))?;
        let lease = g
            .claimed
            .get_mut(&item)
            .ok_or_else(|| format!("lease not active: {lease_id}"))?;
        // Attested-identity gate: a tenant may renew ONLY its own lease.
        if let Some(expected) = expected_holder {
            if lease.holder != expected {
                return Err(holder_mismatch_error(lease_id));
            }
        }
        if lease.expires_at <= now {
            // Stale: expired but not yet swept. Never resurrect it.
            return Err(format!("lease expired: {lease_id}"));
        }
        lease.expires_at = now + Duration::from_millis(ttl_ms);
        Ok(())
    }

    fn ack(
        &self,
        lease_id: &str,
        claim_key: &str,
        expected_holder: Option<&str>,
    ) -> Result<(), String> {
        let mut g = self.inner.lock().expect("store mutex poisoned");
        if let Some(item) = g.lease_index.get(lease_id).cloned() {
            // Attested-identity gate: a tenant may settle ONLY its own lease.
            if let Some(expected) = expected_holder {
                if let Some(lease) = g.claimed.get(&item) {
                    if lease.holder != expected {
                        return Err(holder_mismatch_error(lease_id));
                    }
                }
            }
            g.lease_index.remove(lease_id);
            // Record the lease's OWN claim_key (authoritative — what claim used).
            if let Some(lease) = g.claimed.remove(&item) {
                g.done.insert(lease.claim_key);
            } else {
                g.done.insert(claim_key.to_string());
            }
            g.pending.remove(&item);
            return Ok(());
        }
        // The lease is gone. If this key is already done, the item was acked —
        // idempotent no-op (agentd RFC 0019 §3.5). Otherwise it's an unknown
        // lease: error, and crucially we do NOT fabricate done state from a bare
        // ack (that would let a stray call dedupe-block a legitimate future claim).
        if g.done.contains(claim_key) {
            return Ok(());
        }
        Err(format!("unknown lease_id: {lease_id}"))
    }

    fn release(
        &self,
        lease_id: &str,
        _reason: &str,
        expected_holder: Option<&str>,
    ) -> Result<(), String> {
        let mut g = self.inner.lock().expect("store mutex poisoned");
        let item = g
            .lease_index
            .get(lease_id)
            .cloned()
            .ok_or_else(|| format!("unknown lease_id: {lease_id}"))?;
        // Attested-identity gate: a tenant may release ONLY its own lease.
        if let Some(expected) = expected_holder {
            if let Some(lease) = g.claimed.get(&item) {
                if lease.holder != expected {
                    return Err(holder_mismatch_error(lease_id));
                }
            }
        }
        g.lease_index.remove(lease_id);
        g.claimed.remove(&item);
        g.pending.insert(item, Instant::now());
        Ok(())
    }

    fn sweep_expired(&self) -> usize {
        let mut g = self.inner.lock().expect("store mutex poisoned");
        let now = Instant::now();
        let expired: Vec<(String, String)> = g
            .claimed
            .iter()
            .filter(|(_, lease)| lease.expires_at <= now)
            .map(|(item, lease)| (item.clone(), lease.lease_id.clone()))
            .collect();
        for (item, lease_id) in &expired {
            g.claimed.remove(item);
            g.lease_index.remove(lease_id);
            g.pending.insert(item.clone(), now);
        }
        expired.len()
    }

    fn stats(&self) -> Stats {
        let g = self.inner.lock().expect("store mutex poisoned");
        let now = Instant::now();
        let pending = g.pending.len();
        let claimed = g
            .claimed
            .values()
            .filter(|lease| lease.expires_at > now)
            .count();
        let oldest_age_ms = g
            .pending
            .values()
            .map(|t| now.saturating_duration_since(*t).as_millis() as u64)
            .max()
            .unwrap_or(0);
        Stats {
            pending,
            claimed,
            oldest_age_ms,
        }
    }

    fn pending_items(&self) -> Vec<String> {
        let g = self.inner.lock().expect("store mutex poisoned");
        g.pending.keys().cloned().collect()
    }
}

/// FNV-1a/64 — the same stable hash family the contract uses for sharding
/// (agentd RFC 0019 §4.1). Here it only spices the lease id; uniqueness is the
/// monotonic `seq`, not the hash.
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// An opaque, unique lease id: `lease-<seq>-<hash(seq,item,holder)>`. The
/// monotonic `seq` guarantees uniqueness (no RNG, no wall-clock); the hash makes
/// it opaque/uuid-like so a caller cannot guess or forge another holder's lease.
fn make_lease_id(seq: u64, item: &str, holder: &str) -> String {
    let mix = format!("{seq}\u{0}{item}\u{0}{holder}");
    format!("lease-{seq:08x}-{:016x}", fnv1a64(mix.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};
    use std::thread;

    fn lease_of(r: ClaimResult) -> String {
        match r {
            ClaimResult::Granted { lease_id, .. } => lease_id,
            other => panic!("expected Granted, got {other:?}"),
        }
    }

    // (1) One item, two concurrent racers ⇒ exactly ONE granted, the other
    //     contended (held_by reported). The Mutex is the serializing point.
    #[test]
    fn two_racers_exactly_one_grant() {
        let store: Arc<InMemoryStore> = Arc::new(InMemoryStore::new(4096));
        for i in 0..200 {
            let item = format!("file:///inbox/{i}.json");
            let key = format!("key-{i}");
            let barrier = Arc::new(Barrier::new(2));
            let handles: Vec<_> = (0..2)
                .map(|h| {
                    let store = store.clone();
                    let barrier = barrier.clone();
                    let item = item.clone();
                    let key = key.clone();
                    thread::spawn(move || {
                        barrier.wait();
                        store.claim(&item, 60_000, &key, &format!("holder-{h}"))
                    })
                })
                .collect();
            let results: Vec<ClaimResult> =
                handles.into_iter().map(|h| h.join().unwrap()).collect();
            let granted = results
                .iter()
                .filter(|r| matches!(r, ClaimResult::Granted { .. }))
                .count();
            let contended = results
                .iter()
                .filter(|r| matches!(r, ClaimResult::Contended { held_by: Some(_) }))
                .count();
            assert_eq!(granted, 1, "exactly one racer must win item {i}");
            assert_eq!(
                contended, 1,
                "the loser must see a live holder for item {i}"
            );
        }
    }

    // (2) ack records the claim_key ⇒ a later claim of the SAME key is deduped.
    #[test]
    fn ack_records_claim_key_dedupes() {
        let store = InMemoryStore::new(4096);
        let lease = lease_of(store.claim("item-a", 60_000, "ck-a", "h1"));
        store.ack(&lease, "ck-a", None).expect("ack");
        // Same key again ⇒ never re-granted.
        assert_eq!(
            store.claim("item-a", 60_000, "ck-a", "h2"),
            ClaimResult::Deduped
        );
        // ack is idempotent: re-acking the gone lease, key already done ⇒ Ok.
        assert!(store.ack(&lease, "ck-a", None).is_ok());
    }

    // (3) release returns the item to pending ⇒ immediately re-claimable.
    #[test]
    fn release_returns_to_pending_reclaimable() {
        let store = InMemoryStore::new(4096);
        let lease = lease_of(store.claim("item-r", 60_000, "ck-r", "h1"));
        store.release(&lease, "draining", None).expect("release");
        assert!(store.pending_items().contains(&"item-r".to_string()));
        // Re-claimable (release did NOT record the key as done).
        assert!(matches!(
            store.claim("item-r", 60_000, "ck-r", "h2"),
            ClaimResult::Granted { .. }
        ));
    }

    // (4) Lease expiry returns the item to pending ⇒ re-claimable — both via the
    //     sweep AND lazily on the next claim (correct between sweeps).
    #[test]
    fn lease_expiry_returns_to_pending_reclaimable() {
        let store = InMemoryStore::new(4096);
        // Lazy path: a fresh claim past expiry supersedes without a sweep.
        let _ = store.claim("item-e1", 5, "ck-e1", "h1");
        thread::sleep(Duration::from_millis(40));
        assert!(matches!(
            store.claim("item-e1", 60_000, "ck-e1", "h2"),
            ClaimResult::Granted { .. }
        ));

        // Sweep path: expiry moves the item to pending.
        let _ = store.claim("item-e2", 5, "ck-e2", "h1");
        thread::sleep(Duration::from_millis(40));
        assert!(store.sweep_expired() >= 1);
        assert!(store.pending_items().contains(&"item-e2".to_string()));
        assert!(matches!(
            store.claim("item-e2", 60_000, "ck-e2", "h3"),
            ClaimResult::Granted { .. }
        ));
    }

    // (5) renew extends a live lease; renew/ack/release of an unknown or stale
    //     lease error — and never grant anything.
    #[test]
    fn renew_extends_and_unknown_lease_ops_error() {
        let store = InMemoryStore::new(4096);

        // renew extends: claim short, renew long, sleep past the ORIGINAL ttl,
        // sweep — the item is still held because the renew moved expiry out.
        let lease = lease_of(store.claim("item-x", 30, "ck-x", "h1"));
        store.renew(&lease, 60_000, None).expect("renew live lease");
        thread::sleep(Duration::from_millis(60));
        store.sweep_expired();
        assert!(!store.pending_items().contains(&"item-x".to_string()));
        assert!(matches!(
            store.claim("item-x", 1_000, "ck-x", "h2"),
            ClaimResult::Contended { .. }
        ));

        // Unknown lease ⇒ all three error.
        assert!(store.renew("bogus", 1_000, None).is_err());
        assert!(store.ack("bogus", "no-such-key", None).is_err());
        assert!(store.release("bogus", "x", None).is_err());

        // Stale (expired) lease ⇒ renew errors (never resurrects).
        let stale = lease_of(store.claim("item-s", 5, "ck-s", "h1"));
        thread::sleep(Duration::from_millis(40));
        assert!(store.renew(&stale, 1_000, None).is_err());
    }

    // (6) work.stats counts pending and live-claimed correctly.
    #[test]
    fn stats_counts_pending_and_claimed() {
        let store = InMemoryStore::new(4096);
        assert_eq!(store.submit("p1", Some("kp1")), SubmitOutcome::Enqueued);
        assert_eq!(store.submit("p2", Some("kp2")), SubmitOutcome::Enqueued);
        // Re-submit is a no-op count-wise.
        assert_eq!(
            store.submit("p1", Some("kp1")),
            SubmitOutcome::AlreadyPending
        );
        let _ = store.claim("c1", 60_000, "kc1", "h1");
        let s = store.stats();
        assert_eq!(s.pending, 2);
        assert_eq!(s.claimed, 1);
    }

    // (7) Extra: submit dedupes an already-acked key (the producer-side dedupe).
    #[test]
    fn submit_dedupes_acked_key() {
        let store = InMemoryStore::new(4096);
        let lease = lease_of(store.claim("d1", 60_000, "kd1", "h1"));
        store.ack(&lease, "kd1", None).expect("ack");
        assert_eq!(store.submit("d1", Some("kd1")), SubmitOutcome::Deduped);
        assert_eq!(store.stats().pending, 0);
    }

    // (9) Attested-holder predicate (RFC 0015): a lifecycle op gated with an
    //     `expected_holder` succeeds ONLY when it equals the lease's recorded
    //     holder. A wrong holder is rejected with the holder-mismatch error and the
    //     lease is left untouched (a tenant cannot settle/steal another's lease);
    //     the right holder proceeds. `None` (attest off) is unconstrained.
    #[test]
    fn expected_holder_gates_ack_renew_release() {
        let store = InMemoryStore::new(4096);

        // renew: wrong holder rejected (and recognised as a mismatch), right OK.
        let l_renew = lease_of(store.claim("w-renew", 60_000, "k-renew", "team-a/checkout"));
        let err = store
            .renew(&l_renew, 1_000, Some("team-b/evil"))
            .unwrap_err();
        assert!(is_holder_mismatch(&err), "mismatch error expected: {err}");
        store
            .renew(&l_renew, 1_000, Some("team-a/checkout"))
            .expect("right holder renews");

        // release: wrong holder rejected, the lease stays held (NOT returned).
        let l_rel = lease_of(store.claim("w-rel", 60_000, "k-rel", "team-a/checkout"));
        assert!(is_holder_mismatch(
            &store.release(&l_rel, "x", Some("team-b/evil")).unwrap_err()
        ));
        assert!(
            !store.pending_items().contains(&"w-rel".to_string()),
            "a rejected release must not return the item to pending"
        );
        store
            .release(&l_rel, "drain", Some("team-a/checkout"))
            .expect("right holder releases");
        assert!(store.pending_items().contains(&"w-rel".to_string()));

        // ack: wrong holder rejected, the key is NOT marked done (still claimable
        // by the true holder); the right holder settles + dedupes.
        let l_ack = lease_of(store.claim("w-ack", 60_000, "k-ack", "team-a/checkout"));
        assert!(is_holder_mismatch(
            &store.ack(&l_ack, "k-ack", Some("team-b/evil")).unwrap_err()
        ));
        store
            .ack(&l_ack, "k-ack", Some("team-a/checkout"))
            .expect("right holder acks");
        assert_eq!(
            store.claim("w-ack", 60_000, "k-ack", "team-a/checkout"),
            ClaimResult::Deduped
        );

        // `None` (attest off) is unconstrained — back-compat.
        let l_none = lease_of(store.claim("w-none", 60_000, "k-none", "whoever"));
        store
            .ack(&l_none, "k-none", None)
            .expect("unconstrained ack");
    }

    // (10) The holder-mismatch error is recognised by `is_holder_mismatch` and
    //      distinct from the unknown-lease error (so the wire layer reports a
    //      403-style reject only for true mismatches).
    #[test]
    fn holder_mismatch_error_is_recognised_and_distinct() {
        assert!(is_holder_mismatch(&holder_mismatch_error("lease-x")));
        assert!(holder_mismatch_error("lease-x").contains("lease-x"));
        assert!(!is_holder_mismatch("unknown lease_id: lease-x"));
        assert!(!is_holder_mismatch("lease expired: lease-x"));
    }

    // (8) Extra: the dedupe set honours its FIFO bound (no unbounded growth).
    #[test]
    fn dedupe_set_is_bounded_fifo() {
        let mut d = DoneSet::new(3);
        for k in ["a", "b", "c", "d", "e"] {
            d.insert(k.to_string());
        }
        assert_eq!(d.set.len(), 3);
        // Oldest evicted first.
        assert!(!d.contains("a"));
        assert!(!d.contains("b"));
        assert!(d.contains("c"));
        assert!(d.contains("e"));
    }
}
