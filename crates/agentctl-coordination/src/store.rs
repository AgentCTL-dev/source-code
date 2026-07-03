// SPDX-License-Identifier: BUSL-1.1
//! The claim store — the **single serializing point** (agentctl RFC 0011 §3.2).
//!
//! Everything that makes exactly-one-owner hold lives here, behind ONE `Mutex`:
//! the atomic `work.claim` (exactly one of N concurrent racers for a work unit
//! wins), the lease lifecycle (`renew`/`ack`/`release` + TTL expiry), and the
//! transactional dedupe on `claim_key` (a redelivered-but-already-acked unit is a
//! no-op — the at-least-once safety net, agentd RFC 0019 §3.5).
//!
//! **The unit of exclusivity is the `claim_key`, not the `item` URI.** The
//! `claim_key` is the stable identity of a piece of work; the `item` is the payload
//! (a URI) carried alongside it and reported back in `pending_items`. Grant-one,
//! contention, and the acked-tombstone dedupe are ALL keyed by `claim_key` — the
//! same column the durable [`crate::pg_store::PgClaimStore`] uses as its PRIMARY
//! KEY. This keeps the two backends semantically identical: swapping the in-memory
//! store for Postgres never changes who wins a race. (When `submit` is called
//! without a `claim_key`, the `item` URI is its own key — matching the durable
//! store's `claim_key.unwrap_or(item)`.) In normal claim-mode use each `item` has
//! one stable `claim_key`, so the distinction is invisible; it matters only under
//! contention with reused keys, where both backends now agree.
//!
//! The store sits behind the [`ClaimStore`] trait so a durable backend
//! (Redis/Postgres) can slot in later WITHOUT touching the MCP wire layer; v1
//! ships the in-memory [`InMemoryStore`]. HA / durability of the single replica is
//! the open question recorded in agentctl RFC 0011 §3.2 / §10 — a coordination
//! loss collapses the serializing point for every fleet that depends on it.
//!
//! Time is tracked with [`Instant`] (monotonic) — never wall-clock — so a clock
//! step can never resurrect or prematurely expire a lease.

use std::collections::{HashMap, VecDeque};
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
    /// The `claim_key` was dead-lettered (redelivered past its `max_attempts`
    /// without a terminal ack — RFC 0022 §7). Never re-granted; the wire reports
    /// `held_by:"<deadletter>"`. An admin re-offers it via `work.deadletter`.
    Deadlettered,
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
    /// `claim_key` is in the dead-letter queue — not re-enqueued (RFC 0022 §7).
    Deadlettered,
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
    /// Items dead-lettered (redelivered past `max_attempts`) — awaiting an admin
    /// requeue/drop (RFC 0022 §7).
    pub deadletter: usize,
}

/// The lifecycle state of a work unit, keyed by its `claim_key` — the answer to a
/// `work.result` lookup (RFC 0022 §7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkState {
    /// Enqueued, not yet claimed.
    Pending,
    /// Held under a live lease.
    Claimed,
    /// Terminally acked (a `result` may be attached).
    Done,
    /// Dead-lettered (redelivered past `max_attempts`).
    Deadletter,
    /// No record of this key (never submitted, or evicted past the dedupe horizon).
    Unknown,
}

impl WorkState {
    /// The wire token for this state (`work.result.state`).
    pub fn as_str(self) -> &'static str {
        match self {
            WorkState::Pending => "pending",
            WorkState::Claimed => "claimed",
            WorkState::Done => "done",
            WorkState::Deadletter => "deadletter",
            WorkState::Unknown => "unknown",
        }
    }
}

/// The outcome of a `work.result` lookup: the unit's [`WorkState`] and, when
/// terminally acked, the `result` the worker recorded on ack (RFC 0022 §7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkStatus {
    pub state: WorkState,
    pub result: Option<String>,
}

/// A dead-lettered work unit (surfaced at `dlq://items` / `work.deadletter list`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeadItem {
    pub claim_key: String,
    pub item: String,
    /// How many times it was delivered before being dead-lettered.
    pub attempts: u32,
}

/// The pluggable coordination backend. The MCP wire layer ([`crate::mcp`]) only
/// ever sees this trait, so a durable backend (Redis/Postgres) drops in without a
/// wire change (agentctl RFC 0011 §3.2: the server is pluggable).
///
/// Every method is its own atomic transaction. Implementations MUST serialize
/// `claim` so exactly one of N concurrent racers for the same item is granted.
pub trait ClaimStore: Send + Sync {
    /// Enqueue `item` into the backlog. Skipped if `claim_key` is already done or
    /// dead-lettered. `max_attempts` (from the fleet `workPolicy`, RFC 0022 §7)
    /// bounds redelivery: after that many deliveries without a terminal ack the
    /// item is dead-lettered instead of re-offered. `None` ⇒ unbounded (today).
    fn submit(&self, item: &str, claim_key: Option<&str>, max_attempts: Option<u32>)
        -> SubmitOutcome;
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
        result: Option<&str>,
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
    /// Look up a work unit's state (and its `result`, if terminally acked) by
    /// `claim_key` — the `work.result` correlation read (RFC 0022 §7).
    fn result_of(&self, claim_key: &str) -> WorkStatus;
    /// The current dead-lettered items (for `dlq://items` / `work.deadletter list`).
    fn dead_items(&self) -> Vec<DeadItem>;
    /// Re-offer a dead-lettered item to `pending` (fresh attempt budget). Returns
    /// `true` if the key was in the DLQ, `false` otherwise. (`work.deadletter requeue`.)
    fn requeue_dead(&self, claim_key: &str) -> bool;
    /// Discard a dead-lettered item permanently, tombstoning its key so it is never
    /// re-granted. Returns whether the key was in the DLQ. (`work.deadletter drop`.)
    fn drop_dead(&self, claim_key: &str) -> bool;
}

/// A live lease over one work unit (keyed by its `claim_key`).
#[derive(Debug, Clone)]
struct Lease {
    lease_id: String,
    /// Reported as `held_by` to a contending claimer.
    holder: String,
    /// The dedupe key recorded into the done set on a terminal `ack`. Equals the
    /// `claimed`/`pending` map key.
    claim_key: String,
    /// The payload URI the work unit carries — reported back by `pending_items`
    /// when the unit returns to `pending` (release / lease expiry).
    item: String,
    /// Monotonic expiry. `<= now` ⇒ expired (re-claimable / sweepable).
    expires_at: Instant,
    /// Deliveries so far (incremented on each grant). Redelivery past
    /// `max_attempts` dead-letters the unit (RFC 0022 §7).
    attempts: u32,
    /// Redelivery bound from `work.submit` (the fleet `workPolicy`). `None` ⇒
    /// unbounded (never dead-lettered).
    max_attempts: Option<u32>,
}

/// A pending (enqueued-but-unclaimed) work unit: its payload URI, the instant it
/// entered `pending` (drives `oldest_age_ms` + oldest-first ordering), and its
/// redelivery accounting (carried across the claim/expire cycle).
#[derive(Debug, Clone)]
struct Pending {
    item: String,
    since: Instant,
    attempts: u32,
    max_attempts: Option<u32>,
}

/// A dead-lettered work unit held out of circulation until an admin requeues/drops
/// it (`max_attempts` preserved so a requeue re-enters the same regime).
#[derive(Debug, Clone)]
struct Dead {
    item: String,
    attempts: u32,
    max_attempts: Option<u32>,
}

/// A bounded FIFO dedupe map of acked `claim_key`s → their (optional) `result`.
///
/// **The bound (documented):** capped at `cap` entries; when full, the OLDEST key
/// is evicted first (insertion order). A redelivery of an item whose key was
/// evicted could be re-granted — acceptable because (a) the cap is large
/// (default 100k) so the eviction horizon is far past any realistic
/// in-flight/redelivery window, and (b) the downstream side effect is itself
/// `claim_key`-idempotent (agentd RFC 0019 §3.5), so a re-grant past the horizon
/// re-collapses to one effect. This keeps the map from growing unbounded. A result
/// recorded on ack is retrievable via `work.result` until its key is evicted.
#[derive(Debug)]
struct DoneSet {
    map: HashMap<String, Option<String>>,
    order: VecDeque<String>,
    cap: usize,
}

impl DoneSet {
    fn new(cap: usize) -> Self {
        Self {
            map: HashMap::new(),
            order: VecDeque::new(),
            cap: cap.max(1),
        }
    }

    fn contains(&self, key: &str) -> bool {
        self.map.contains_key(key)
    }

    /// The recorded result for a done key (`Some(None)` = acked with no result;
    /// `None` = key not done).
    fn get(&self, key: &str) -> Option<&Option<String>> {
        self.map.get(key)
    }

    fn insert(&mut self, key: String, result: Option<String>) {
        if let Some(slot) = self.map.get_mut(&key) {
            // Already done; keep eviction order. Record a result if one arrived and
            // none was stored (idempotent re-ack without a result won't wipe it).
            if slot.is_none() && result.is_some() {
                *slot = result;
            }
            return;
        }
        self.map.insert(key.clone(), result);
        self.order.push_back(key);
        while self.map.len() > self.cap {
            match self.order.pop_front() {
                Some(old) => {
                    self.map.remove(&old);
                }
                None => break,
            }
        }
    }
}

/// Everything guarded by the single store mutex. Every map is keyed by
/// `claim_key` — the unit of exclusivity (see the module docs) — so grant-one,
/// contention, and dedupe all serialize on the same key the durable store uses.
#[derive(Debug)]
struct Inner {
    /// claim_key → its enqueued-but-unclaimed work unit (payload + enqueue instant).
    pending: HashMap<String, Pending>,
    /// claim_key → its live lease.
    claimed: HashMap<String, Lease>,
    /// lease_id → claim_key (reverse lookup for renew/ack/release).
    lease_index: HashMap<String, String>,
    /// The bounded dedupe map of acked `claim_key`s → result.
    done: DoneSet,
    /// claim_key → dead-lettered unit (redelivered past `max_attempts`).
    deadletter: HashMap<String, Dead>,
    /// Monotonic lease-id counter (uniqueness without wall-clock / RNG).
    seq: u64,
}

impl Inner {
    /// Return an expired lease's unit to `pending`, OR dead-letter it when it has
    /// been delivered past its `max_attempts` (RFC 0022 §7). Shared by
    /// `sweep_expired`, the lazy-reclaim branch of `claim`, and `release`. Returns
    /// `true` if the unit was dead-lettered.
    fn retire_or_requeue(&mut self, key: String, item: String, attempts: u32, max: Option<u32>, now: Instant) -> bool {
        if max.is_some_and(|m| attempts >= m) {
            self.deadletter.insert(
                key,
                Dead {
                    item,
                    attempts,
                    max_attempts: max,
                },
            );
            true
        } else {
            self.pending.insert(
                key,
                Pending {
                    item,
                    since: now,
                    attempts,
                    max_attempts: max,
                },
            );
            false
        }
    }
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
                deadletter: HashMap::new(),
                seq: 0,
            }),
        }
    }
}

impl ClaimStore for InMemoryStore {
    fn submit(
        &self,
        item: &str,
        claim_key: Option<&str>,
        max_attempts: Option<u32>,
    ) -> SubmitOutcome {
        let mut g = self.inner.lock().expect("store mutex poisoned");
        let now = Instant::now();
        // The claim_key is the work identity; when absent the item URI is its own
        // key (matches the durable store's `claim_key.unwrap_or(item)`).
        let key = claim_key.unwrap_or(item);
        // Classify against the existing state in the same precedence the durable
        // store uses (acked tombstone → dead-letter → claimed → pending). A claimed
        // entry reports AlreadyClaimed whether or not its lease has expired: an
        // expired-but-unswept lease is reclaimed lazily by the next `claim` (or the
        // sweeper), so a producer re-submit is a transient no-op, never a second
        // enqueue.
        if g.done.contains(key) {
            return SubmitOutcome::Deduped;
        }
        if g.deadletter.contains_key(key) {
            return SubmitOutcome::Deadlettered;
        }
        if g.claimed.contains_key(key) {
            return SubmitOutcome::AlreadyClaimed;
        }
        if g.pending.contains_key(key) {
            return SubmitOutcome::AlreadyPending;
        }
        g.pending.insert(
            key.to_string(),
            Pending {
                item: item.to_string(),
                since: now,
                attempts: 0,
                max_attempts,
            },
        );
        SubmitOutcome::Enqueued
    }

    fn claim(&self, item: &str, ttl_ms: u64, claim_key: &str, holder: &str) -> ClaimResult {
        let mut g = self.inner.lock().expect("store mutex poisoned");
        let now = Instant::now();

        // DEDUPE first: an already-acked key is never re-granted.
        if g.done.contains(claim_key) {
            return ClaimResult::Deduped;
        }
        // Dead-lettered: never re-granted until an admin requeues it.
        if g.deadletter.contains_key(claim_key) {
            return ClaimResult::Deadlettered;
        }

        // A live lease for this key ⇒ contention. (An expired lease falls through
        // and is superseded below — lazy reclaim, correct between sweeps.)
        if let Some(lease) = g.claimed.get(claim_key) {
            if lease.expires_at > now {
                return ClaimResult::Contended {
                    held_by: Some(lease.holder.clone()),
                };
            }
        }
        // Redelivery accounting carried across the claim/expire cycle. An expired
        // lease being superseded here (lazy reclaim) is itself a failed delivery: if
        // it is already past `max_attempts`, dead-letter it instead of re-granting.
        let prior = if let Some(old) = g.claimed.remove(claim_key) {
            g.lease_index.remove(&old.lease_id);
            if old.max_attempts.is_some_and(|m| old.attempts >= m) {
                g.retire_or_requeue(claim_key.to_string(), old.item, old.attempts, old.max_attempts, now);
                return ClaimResult::Deadlettered;
            }
            (old.attempts, old.max_attempts)
        } else if let Some(p) = g.pending.get(claim_key) {
            (p.attempts, p.max_attempts)
        } else {
            // A direct claim of a never-submitted key: no attempt budget (unbounded).
            (0, None)
        };

        // GRANT — this delivery increments the attempt count.
        g.seq += 1;
        let seq = g.seq;
        let lease_id = make_lease_id(seq, item, holder);
        let lease = Lease {
            lease_id: lease_id.clone(),
            holder: holder.to_string(),
            claim_key: claim_key.to_string(),
            item: item.to_string(),
            expires_at: now + Duration::from_millis(ttl_ms),
            attempts: prior.0 + 1,
            max_attempts: prior.1,
        };
        g.claimed.insert(claim_key.to_string(), lease);
        g.lease_index.insert(lease_id.clone(), claim_key.to_string());
        g.pending.remove(claim_key);
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
        let key = g
            .lease_index
            .get(lease_id)
            .cloned()
            .ok_or_else(|| format!("unknown lease_id: {lease_id}"))?;
        let lease = g
            .claimed
            .get_mut(&key)
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
        result: Option<&str>,
    ) -> Result<(), String> {
        let mut g = self.inner.lock().expect("store mutex poisoned");
        if let Some(key) = g.lease_index.get(lease_id).cloned() {
            // Attested-identity gate: a tenant may settle ONLY its own lease.
            if let Some(expected) = expected_holder {
                if let Some(lease) = g.claimed.get(&key) {
                    if lease.holder != expected {
                        return Err(holder_mismatch_error(lease_id));
                    }
                }
            }
            g.lease_index.remove(lease_id);
            let result = result.map(str::to_string);
            // Record the lease's OWN claim_key (authoritative — what claim used; it
            // equals `key`) plus the settle `result`. Falls back to the passed key
            // if the entry vanished.
            if let Some(lease) = g.claimed.remove(&key) {
                g.done.insert(lease.claim_key, result);
            } else {
                g.done.insert(claim_key.to_string(), result);
            }
            g.pending.remove(&key);
            return Ok(());
        }
        // The lease is gone. If this key is already done, the item was acked —
        // idempotent no-op (agentd RFC 0019 §3.5); a late `result` on the re-ack is
        // recorded only if none was stored. Otherwise it's an unknown lease: error,
        // and crucially we do NOT fabricate done state from a bare ack (that would
        // let a stray call dedupe-block a legitimate future claim).
        if g.done.contains(claim_key) {
            if let Some(r) = result {
                g.done.insert(claim_key.to_string(), Some(r.to_string()));
            }
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
        let now = Instant::now();
        let key = g
            .lease_index
            .get(lease_id)
            .cloned()
            .ok_or_else(|| format!("unknown lease_id: {lease_id}"))?;
        // Attested-identity gate: a tenant may release ONLY its own lease.
        if let Some(expected) = expected_holder {
            if let Some(lease) = g.claimed.get(&key) {
                if lease.holder != expected {
                    return Err(holder_mismatch_error(lease_id));
                }
            }
        }
        g.lease_index.remove(lease_id);
        // Return the unit to pending (carrying its payload URI + redelivery
        // accounting), OR dead-letter it if this delivery pushed it past
        // `max_attempts` (a voluntarily-released poison item still counts).
        let (item, attempts, max) = g
            .claimed
            .remove(&key)
            .map(|l| (l.item, l.attempts, l.max_attempts))
            .unwrap_or_else(|| (key.clone(), 0, None));
        g.retire_or_requeue(key, item, attempts, max, now);
        Ok(())
    }

    fn sweep_expired(&self) -> usize {
        let mut g = self.inner.lock().expect("store mutex poisoned");
        let now = Instant::now();
        let expired: Vec<(String, String, String, u32, Option<u32>)> = g
            .claimed
            .iter()
            .filter(|(_, lease)| lease.expires_at <= now)
            .map(|(key, lease)| {
                (
                    key.clone(),
                    lease.lease_id.clone(),
                    lease.item.clone(),
                    lease.attempts,
                    lease.max_attempts,
                )
            })
            .collect();
        for (key, lease_id, item, attempts, max) in &expired {
            g.claimed.remove(key);
            g.lease_index.remove(lease_id);
            // Return to pending, OR dead-letter when past the redelivery bound.
            g.retire_or_requeue(key.clone(), item.clone(), *attempts, *max, now);
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
            .map(|p| now.saturating_duration_since(p.since).as_millis() as u64)
            .max()
            .unwrap_or(0);
        Stats {
            pending,
            claimed,
            oldest_age_ms,
            deadletter: g.deadletter.len(),
        }
    }

    fn pending_items(&self) -> Vec<String> {
        let g = self.inner.lock().expect("store mutex poisoned");
        // Oldest-first, matching the durable store's `ORDER BY updated_at ASC`, so
        // the `work://pending` resource reads identically on both backends.
        let mut entries: Vec<&Pending> = g.pending.values().collect();
        entries.sort_by_key(|p| p.since);
        entries.into_iter().map(|p| p.item.clone()).collect()
    }

    fn result_of(&self, claim_key: &str) -> WorkStatus {
        let g = self.inner.lock().expect("store mutex poisoned");
        let now = Instant::now();
        // Precedence mirrors the write paths: done (terminal) → dead-letter →
        // live-claimed → pending → unknown.
        if let Some(result) = g.done.get(claim_key) {
            return WorkStatus {
                state: WorkState::Done,
                result: result.clone(),
            };
        }
        if g.deadletter.contains_key(claim_key) {
            return WorkStatus {
                state: WorkState::Deadletter,
                result: None,
            };
        }
        if g
            .claimed
            .get(claim_key)
            .is_some_and(|l| l.expires_at > now)
        {
            return WorkStatus {
                state: WorkState::Claimed,
                result: None,
            };
        }
        if g.pending.contains_key(claim_key) || g.claimed.contains_key(claim_key) {
            // A pending item, or an expired-but-unswept lease (about to be re-offered).
            return WorkStatus {
                state: WorkState::Pending,
                result: None,
            };
        }
        WorkStatus {
            state: WorkState::Unknown,
            result: None,
        }
    }

    fn dead_items(&self) -> Vec<DeadItem> {
        let g = self.inner.lock().expect("store mutex poisoned");
        let mut items: Vec<DeadItem> = g
            .deadletter
            .iter()
            .map(|(key, d)| DeadItem {
                claim_key: key.clone(),
                item: d.item.clone(),
                attempts: d.attempts,
            })
            .collect();
        items.sort_by(|a, b| a.claim_key.cmp(&b.claim_key));
        items
    }

    fn requeue_dead(&self, claim_key: &str) -> bool {
        let mut g = self.inner.lock().expect("store mutex poisoned");
        let now = Instant::now();
        match g.deadletter.remove(claim_key) {
            Some(d) => {
                // Re-offer with a FRESH attempt budget (the admin chose to retry).
                g.pending.insert(
                    claim_key.to_string(),
                    Pending {
                        item: d.item,
                        since: now,
                        attempts: 0,
                        max_attempts: d.max_attempts,
                    },
                );
                true
            }
            None => false,
        }
    }

    fn drop_dead(&self, claim_key: &str) -> bool {
        let mut g = self.inner.lock().expect("store mutex poisoned");
        match g.deadletter.remove(claim_key) {
            Some(_) => {
                // Tombstone the key so it is never re-granted (a dropped poison item
                // must not resurrect on a re-submit); no result.
                g.done.insert(claim_key.to_string(), None);
                true
            }
            None => false,
        }
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
        store.ack(&lease, "ck-a", None, None).expect("ack");
        // Same key again ⇒ never re-granted.
        assert_eq!(
            store.claim("item-a", 60_000, "ck-a", "h2"),
            ClaimResult::Deduped
        );
        // ack is idempotent: re-acking the gone lease, key already done ⇒ Ok.
        assert!(store.ack(&lease, "ck-a", None, None).is_ok());
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
        assert!(store.ack("bogus", "no-such-key", None, None).is_err());
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
        assert_eq!(store.submit("p1", Some("kp1"), None), SubmitOutcome::Enqueued);
        assert_eq!(store.submit("p2", Some("kp2"), None), SubmitOutcome::Enqueued);
        // Re-submit is a no-op count-wise.
        assert_eq!(
            store.submit("p1", Some("kp1"), None),
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
        store.ack(&lease, "kd1", None, None).expect("ack");
        assert_eq!(store.submit("d1", Some("kd1"), None), SubmitOutcome::Deduped);
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
            &store.ack(&l_ack, "k-ack", Some("team-b/evil"), None).unwrap_err()
        ));
        store
            .ack(&l_ack, "k-ack", Some("team-a/checkout"), None)
            .expect("right holder acks");
        assert_eq!(
            store.claim("w-ack", 60_000, "k-ack", "team-a/checkout"),
            ClaimResult::Deduped
        );

        // `None` (attest off) is unconstrained — back-compat.
        let l_none = lease_of(store.claim("w-none", 60_000, "k-none", "whoever"));
        store
            .ack(&l_none, "k-none", None, None)
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

    // (11) Canonical cross-backend semantics: the unit of exclusivity is the
    //      claim_key, NOT the item URI. Two DIFFERENT keys on the SAME item URI are
    //      two independent work units — both grant. (Historically the in-memory
    //      store keyed contention by item and would have wrongly contended the
    //      second; the durable Postgres store keys by claim_key. Both now agree.)
    #[test]
    fn distinct_keys_same_item_are_independent_units() {
        let store = InMemoryStore::new(4096);
        assert!(matches!(
            store.claim("file:///x", 60_000, "k1", "h1"),
            ClaimResult::Granted { .. }
        ));
        assert!(
            matches!(
                store.claim("file:///x", 60_000, "k2", "h2"),
                ClaimResult::Granted { .. }
            ),
            "a distinct claim_key is a distinct work unit even on the same item URI"
        );
    }

    // (12) The converse: the SAME claim_key is ONE work unit regardless of the item
    //      URI presented — exactly one of N concurrent racers wins, the rest are
    //      contended. This is the grant-one invariant the durable store enforces via
    //      its `claim_key` PRIMARY KEY, mirrored here.
    #[test]
    fn same_key_distinct_items_grant_exactly_one() {
        let store: Arc<InMemoryStore> = Arc::new(InMemoryStore::new(4096));
        let barrier = Arc::new(Barrier::new(4));
        let handles: Vec<_> = (0..4)
            .map(|h| {
                let store = store.clone();
                let barrier = barrier.clone();
                thread::spawn(move || {
                    barrier.wait();
                    // Same key, four different item URIs, four holders.
                    store.claim(&format!("file:///item-{h}"), 60_000, "shared-key", &format!("h{h}"))
                })
            })
            .collect();
        let results: Vec<ClaimResult> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let granted = results
            .iter()
            .filter(|r| matches!(r, ClaimResult::Granted { .. }))
            .count();
        assert_eq!(granted, 1, "exactly one racer wins the shared claim_key");
        assert_eq!(
            results
                .iter()
                .filter(|r| matches!(r, ClaimResult::Contended { .. }))
                .count(),
            3,
            "the other three see the shared key as contended"
        );
    }

    // (13) pending_items reports the ITEM payload (URI), not the claim_key, and
    //      oldest-first — matching the durable store's `SELECT item ... ORDER BY
    //      updated_at ASC`. Submit two units (distinct key/item), assert order + payload.
    #[test]
    fn pending_items_reports_item_payload_oldest_first() {
        let store = InMemoryStore::new(4096);
        assert_eq!(
            store.submit("file:///a", Some("k-a"), None),
            SubmitOutcome::Enqueued
        );
        // Distinct enqueue instants so the ordering is deterministic.
        thread::sleep(Duration::from_millis(5));
        assert_eq!(
            store.submit("file:///b", Some("k-b"), None),
            SubmitOutcome::Enqueued
        );
        assert_eq!(
            store.pending_items(),
            vec!["file:///a".to_string(), "file:///b".to_string()],
            "items (not keys), oldest first"
        );
    }

    // (14) submit reports AlreadyClaimed for an item under a live OR expired-unswept
    //      lease (matching the durable store, which does not check expiry in submit —
    //      the next claim / the sweeper reclaims it). No second enqueue either way.
    #[test]
    fn submit_of_claimed_key_is_already_claimed_even_if_expired() {
        let store = InMemoryStore::new(4096);
        let _ = store.claim("file:///c", 5, "k-c", "h1");
        thread::sleep(Duration::from_millis(40)); // lease now expired, not swept
        assert_eq!(
            store.submit("file:///c", Some("k-c"), None),
            SubmitOutcome::AlreadyClaimed
        );
        assert_eq!(store.stats().pending, 0, "no phantom re-enqueue");
    }

    // (15) RFC 0022 §7: ack records a result; work.result retrieves it. The state
    //      machine reports pending → claimed → done across the lifecycle.
    #[test]
    fn ack_result_is_recorded_and_retrievable() {
        let store = InMemoryStore::new(4096);
        // Unknown before submit.
        assert_eq!(store.result_of("wk").state, WorkState::Unknown);
        assert_eq!(store.submit("file:///w", Some("wk"), None), SubmitOutcome::Enqueued);
        assert_eq!(store.result_of("wk").state, WorkState::Pending);
        let lease = lease_of(store.claim("file:///w", 60_000, "wk", "h1"));
        assert_eq!(store.result_of("wk").state, WorkState::Claimed);
        store
            .ack(&lease, "wk", None, Some("{\"answer\":42}"))
            .expect("ack with result");
        let done = store.result_of("wk");
        assert_eq!(done.state, WorkState::Done);
        assert_eq!(done.result.as_deref(), Some("{\"answer\":42}"));
    }

    // (16) Dead-letter: with max_attempts=1, the first delivery's expiry retires the
    //      item to the DLQ instead of re-offering it. It is never re-granted, shows
    //      up in dead_items, and requeue/drop manage it.
    #[test]
    fn poison_item_dead_letters_after_max_attempts() {
        let store = InMemoryStore::new(4096);
        assert_eq!(
            store.submit("file:///poison", Some("pk"), Some(1)),
            SubmitOutcome::Enqueued
        );
        // Delivery #1.
        let _ = store.claim("file:///poison", 5, "pk", "h1");
        thread::sleep(Duration::from_millis(40));
        // The sweep past expiry retires it to the DLQ (attempts 1 >= max 1).
        assert_eq!(store.sweep_expired(), 1);
        assert_eq!(store.stats().deadletter, 1);
        assert_eq!(store.stats().pending, 0, "not re-offered");
        // Never re-granted while dead-lettered.
        assert_eq!(
            store.claim("file:///poison", 60_000, "pk", "h2"),
            ClaimResult::Deadlettered
        );
        assert_eq!(store.result_of("pk").state, WorkState::Deadletter);
        // Surfaced for an admin.
        let dead = store.dead_items();
        assert_eq!(dead.len(), 1);
        assert_eq!(dead[0].claim_key, "pk");
        assert_eq!(dead[0].attempts, 1);
        // Requeue re-offers with a fresh budget.
        assert!(store.requeue_dead("pk"));
        assert!(!store.requeue_dead("pk"), "already requeued");
        assert!(store.pending_items().contains(&"file:///poison".to_string()));
        assert!(matches!(
            store.claim("file:///poison", 60_000, "pk", "h3"),
            ClaimResult::Granted { .. }
        ));
    }

    // (17) A dead-lettered item that is DROPPED is tombstoned — a re-submit is
    //      deduped, never resurrecting the poison item.
    #[test]
    fn dropped_dead_item_is_tombstoned() {
        let store = InMemoryStore::new(4096);
        let _ = store.submit("file:///d", Some("dk"), Some(1));
        let _ = store.claim("file:///d", 5, "dk", "h1");
        thread::sleep(Duration::from_millis(40));
        store.sweep_expired();
        assert!(store.drop_dead("dk"));
        assert!(!store.drop_dead("dk"));
        // Tombstoned: a re-submit is deduped, a re-claim is deduped.
        assert_eq!(store.submit("file:///d", Some("dk"), None), SubmitOutcome::Deduped);
        assert_eq!(store.claim("file:///d", 60_000, "dk", "h2"), ClaimResult::Deduped);
    }

    // (18) Under budget (max_attempts=3), an expired item is redelivered, not
    //      dead-lettered, until the budget is exhausted.
    #[test]
    fn redelivery_under_budget_returns_to_pending() {
        let store = InMemoryStore::new(4096);
        let _ = store.submit("file:///r", Some("rk"), Some(3));
        // Two delivery/expiry cycles stay under the budget of 3.
        for _ in 0..2 {
            let _ = store.claim("file:///r", 5, "rk", "h");
            thread::sleep(Duration::from_millis(20));
            store.sweep_expired();
        }
        assert_eq!(store.stats().deadletter, 0, "still under budget");
        assert!(store.pending_items().contains(&"file:///r".to_string()));
    }

    // (8) Extra: the dedupe set honours its FIFO bound (no unbounded growth).
    #[test]
    fn dedupe_set_is_bounded_fifo() {
        let mut d = DoneSet::new(3);
        for k in ["a", "b", "c", "d", "e"] {
            d.insert(k.to_string(), None);
        }
        assert_eq!(d.map.len(), 3);
        // Oldest evicted first.
        assert!(!d.contains("a"));
        assert!(!d.contains("b"));
        assert!(d.contains("c"));
        assert!(d.contains("e"));
    }
}
