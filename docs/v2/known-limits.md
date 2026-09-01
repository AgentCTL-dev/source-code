# Known limits

Operational ceilings, with the evidence that found them and the state of the
fix. None are data-loss bugs — the durable writes are always sound.

## State read-back and the 256 KiB FFI frame — FIXED (control plane)

**The limit.** A `dev.mcpg.backend.sql` tool returns its result to the mcpg host
over one FFI frame with a **262 144-byte (256 KiB) payload cap** (an mcpg host
constant, not configurable from our config). Any read that put a whole prefix —
or even one large value — into a single result therefore had a ceiling:
`state.admin.snapshot` / `state.get` over the cap returned a tool-execution
error, not rows:

```
isError: true
"backend plugin returned an FFI payload of 531159 bytes exceeding the host cap of 262144 bytes"
```

The write direction has no such 256 KiB cap — a `state.put` of a 500 KiB value is
accepted (verified live). It is bounded only by the state gateway's
`max_request_body_mb: 4`. So a value could be *written* and then be unreadable —
a real inaccessibility, not just a backup nicety.

**Why a prefix grows.** A `store.class: managed` daemon checkpoints under
`orgs/<ns>/<agent>/<pod>/…`: `manifest`/`context` per pod (rewritten in place),
plus `run/<workflow>-<runid>` — **one new key per loop tick**, never GC'd, and
orphaned per-pod on every restart (the Agent CR's deletion does not purge the
store, so `restore`/`migrate` can run against a dead pod).

**Discovered by** the 2026-09-01 `state-durability` investigation: a reused agent
name accumulated 302 keys / 531 KiB, the snapshot errored, and the null result
read as "no checkpoints" — which first looked like an agentd checkpoint gap. It
was not; agentd's writes were correct, only the read-back overflowed the frame.

**Fix — shipped, agentctl-side, verified live on `backend-sql:protocol-1`.**
Two independent axes, so **no value size and no prefix size is off-limits**:

- **Across keys — keyset pagination.** `state.admin.snapshot` / `state.admin.list`
  take `after` / `limit` and return `next`; a walk (`after = next`) pages a whole
  prefix over many under-cap frames. `state.admin.list` is keys-only
  (`{key, seq}`, no state blob), so existence checks and `migrate`'s seq compare
  never pull state at all.
- **Within one value — chunked read.** `state.admin.read_chunk {key, offset,
  length}` returns `substring(state::text …)` + total `len`; concatenating the
  ordered char-slices reproduces the value byte-for-byte (md5-verified), then it
  parses as JSON. A single value of *any* size streams back over as many
  under-cap frames as it needs.
- **The verbs use both.** `agentctl backup`'s `snapshot_all` batches via
  paginated snapshot, HALVES `limit` when a page is over the frame, and — for one
  value too big even at `limit 1` — streams it with `read_chunked`. `restore`
  UPSERTs the backup in batches under the gateway body. The apiserver state
  client and the e2e helper now surface `result.isError` instead of reading a
  capped payload as empty.

Verified: a 552 KiB / 400-key prefix backs up completely; a prefix holding a
single **700 KiB** value (2.7× the frame) backs up, restores, and comes back
byte-intact; the `state-durability` e2e round-trips a ~400 KiB value through
`read_chunk`. There is no longer a per-value or per-prefix read-back size limit
on the control-plane path.

**Residual A — the write/restore body (configurable, not the FFI frame).** A
single value must fit the state gateway's request body (`max_request_body_mb`,
default 4 MiB) to be written or restored. This is the SAME channel agentd writes
through, so it is not an agentctl-imposed limit: if agentd could write the
value, we can back it up and restore it. Raise `max_request_body_mb` to lift it;
values beyond a single body would need chunked *write* (staged append), which
nothing today requires.

**Residual B — agentd's own runtime `state.get` (data-plane).** agentd restores a
checkpoint at boot with a single `state.get`, which is one frame — so a
checkpoint agentd itself wrote larger than 256 KiB (a big `context`, say) would
fail agentd's own restore. Only agentd can fix this (chunk its restore reads, or
cap an individual checkpoint's size); tracked with the run-cursor GC ask (PLAN
U10). agentctl's backup/migrate of such a value already works via `read_chunk`.
