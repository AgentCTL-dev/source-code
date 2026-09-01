# Known limits

Operational ceilings that are correct-but-bounded today, with the evidence that
found them and the shape of the real fix. None of these are data-loss bugs — the
durable writes are always sound; the limits are on **bulk read-back** paths.

## `state.admin.snapshot` — 256 KiB whole-prefix export cap

**Limit.** `state.admin.snapshot` (and the `agentctl backup` / `migrate` verbs and
the `state-durability` e2e that ride it) exports **every** `{key, seq, state}`
under a prefix as a single `jsonb_agg` result. mcpg returns a SQL backend result
over one FFI frame with a **host payload cap of 262 144 bytes (256 KiB)**. A
prefix whose full state exceeds that returns a tool-execution error, not rows:

```
isError: true
"sql request failed: binding transport error:
 backend plugin returned an FFI payload of 531159 bytes
 exceeding the host cap of 262144 bytes"
```

**Why a prefix grows.** A `store.class: managed` daemon checkpoints its durable
workflow state under `orgs/<ns>/<agent>/<pod>/…`:

- `manifest/agent` and `context/root` — one per pod, small, rewritten in place.
- `run/<workflow>-<runid>` — **one new key per loop tick / run**, and agentd does
  **not** GC completed run cursors. Over a long-lived agent these accumulate
  without bound.
- Keys are namespaced by **pod name**, so every restart/reschedule leaves the
  previous pod's keys orphaned under the shared agent prefix (the Agent CR's
  deletion does not purge the state store — checkpoints outlive the workload by
  design, so `restore`/`migrate` can run against a dead pod).

So the export payload is `O(total checkpoints ever written under the prefix)`, and
any sufficiently long-lived or frequently-recycled managed agent will eventually
cross the cap — at which point `backup`/`migrate`/the existence check all fail.

**Discovered by** the 2026-09-01 `state-durability` investigation: the scenario
reused the `state-probe` agent name across ~18 runs without purging, accumulating
302 keys / 531 KiB under one prefix. agentd's checkpointing was correct throughout
(the writes are fenced and durable); only the whole-prefix *read-back* blew the
cap. A fresh agent's prefix (single pod, a handful of run keys) is well under it —
which is why the same scenario passed at 34 s when the prefix was small.

**Interim mitigations (in place).**
- The `state-durability` e2e purges the agent prefix at the start of Leg 2
  (`state.admin.purge`) so a reused name starts hermetic, and its existence
  checks now surface an `isError` snapshot **loudly** (`snapshot_items`) instead
  of reading a capped payload as "no checkpoints" and timing out — the failure
  that originally masqueraded as an agentd checkpoint gap.
- `agentctl backup` should be run against agents whose live prefix is under the
  cap; there is no partial-export fallback yet.

**Real fix (not yet implemented).** Pick per need:
- a **keys-only / existence** mode on `state.admin.snapshot` (return `{key, seq}`
  without `state`) for existence checks and listings — payload becomes
  `O(key count)`, orders of magnitude smaller, and covers the e2e/backup-probe
  case immediately;
- **paginated / streamed** export (cursor over `key`, chunk under the FFI cap)
  for a complete backup of an arbitrarily large prefix;
- **run-cursor GC** on the agentd side (retain only the live/last-N run cursors)
  so a prefix's steady-state size is bounded regardless of uptime.

The first two are agentctl-side (state gateway config + verb); the third is an
agentd/data-plane change. Until one lands, treat 256 KiB of per-prefix state as
the backup ceiling.
