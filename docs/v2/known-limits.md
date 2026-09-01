# Known limits

Operational ceilings that are correct-but-bounded, with the evidence that found
them and the state of the fix. None are data-loss bugs — the durable writes are
always sound; the limits are on **bulk read-back**.

## State read-back and the 256 KiB FFI frame — MITIGATED

**The limit.** A `dev.mcpg.backend.sql` tool returns its result to the mcpg host
over one FFI frame with a **262 144-byte (256 KiB) payload cap**. Any tool that
aggregated a whole key prefix into one result (`state.admin.snapshot`, and the
`agentctl backup` / `migrate` verbs and the `state-durability` existence check
that rode it) therefore had an implicit ceiling: you can *write* more state under
a prefix than you could read back in one call. Over the cap the tool returns a
tool-execution error, not rows:

```
isError: true
"backend plugin returned an FFI payload of 531159 bytes exceeding the host cap of 262144 bytes"
```

**Why a prefix grows.** A `store.class: managed` daemon checkpoints under
`orgs/<ns>/<agent>/<pod>/…`:

- `manifest/agent`, `context/root` — one per pod, rewritten in place.
- `run/<workflow>-<runid>` — **one new key per loop tick / run**, and agentd does
  **not** GC completed run cursors → unbounded over uptime.
- Keys are namespaced by pod name, so every restart/reschedule orphans the old
  pod's keys under the shared agent prefix (deleting the Agent CR does not purge
  the store — checkpoints outlive the workload so `restore`/`migrate` can run
  against a dead pod).

So the export payload was `O(total checkpoints ever written under the prefix)`.

**Discovered by** the 2026-09-01 `state-durability` investigation: a reused agent
name accumulated 302 keys / 531 KiB, the snapshot errored, and the check read the
null `structuredContent.items` as "no checkpoints" and timed out — which
initially looked like an agentd checkpoint gap. agentd's checkpointing was
correct throughout; only the whole-prefix *read-back* blew the frame.

**Fix — shipped (agentctl-side).**
- **Keys-only enumeration** — a new `state.admin.list` returns `{key, seq}`
  *without* the state blob. A key row is ~100 B vs a ~KiB envelope, so the same
  key count clears the frame by orders of magnitude. The existence check and
  `migrate`'s zero-loss seq compare use it; both never pull state.
- **Keyset pagination** — `state.admin.snapshot` (and `state.admin.list`) now
  take `after` / `limit` and return `next` (the page's last key; null when
  empty). `agentctl backup` walks pages (`after = next`) until an empty page and
  concatenates — an arbitrarily large prefix backs up over many under-cap frames.
  `limit` defaults to 100 for snapshot (conservative for the ~KiB envelopes) and
  1000 for the keys-only list.
- **Errors surface** — both the apiserver's state client and the e2e helper now
  fail on `result.isError` instead of reading a capped payload as an empty
  result (the silent mode that started the whole investigation).

Verified live against the real `backend-sql:protocol-1` plugin: a 552 KiB /
400-key prefix backs up completely (all 400 rows over multiple pages), keys-only
lists it in one small page, and a deliberately over-cap single page still errors.

**One residual invariant.** A *single* state envelope must be < the 256 KiB frame
(agentd's are ~1–6 KiB) — pagination bounds a page's row *count*, not one row's
size. This holds for agentd; a backend that stored a multi-hundred-KiB envelope
would need byte-budgeted paging instead.

**Still open — agentd-side (upstream ask).** Pagination makes the read-back
tolerate unbounded growth; it does not stop the store from growing. **Run-cursor
GC in agentd** (retain only live / last-N run cursors) is the only fix that keeps
a prefix's steady-state size bounded by *concurrent* runs rather than uptime.
Tracked as an upstream agentd ask (docs/v2/PLAN.md, U-series); until it lands a
managed agent's prefix grows one key per tick, and backup/list cost grows with
it (correctly, just not for free).
