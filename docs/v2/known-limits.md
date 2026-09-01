# Known limits

Operational ceilings, with the evidence that found them and the state of the
fix. None are data-loss bugs — the durable writes are always sound.

## State value/prefix size — FIXED

**The limit.** A `dev.mcpg.backend.sql` tool returns its result to the mcpg host
over one FFI frame. That frame has a per-plugin payload cap — `ffi_limits.
max_payload_bytes`, **host default 262 144 bytes (256 KiB)**. Any read that put a
whole prefix or a large value into one result over the cap failed with a
tool-execution error, not rows:

```
isError: true
"backend plugin returned an FFI payload of 531159 bytes exceeding the host cap of 262144 bytes"
```

The write side has no FFI cap (arguments flow to the plugin uncapped); it is
bounded only by the gateway's `server.max_request_body_mb`.

**Discovered by** the 2026-09-01 `state-durability` investigation: a reused agent
name accumulated 302 keys / 531 KiB, the snapshot errored, and the null result
read as "no checkpoints" — first mistaken for an agentd checkpoint gap. It was
not; agentd's writes were correct, only the read-back overflowed the frame.

**Fix — two layers, so there is no size limit in practice or in principle.**

1. **Raise the cap (primary path).** The FFI cap is a per-plugin operator
   override, confirmed by mcpg and verified live. The state gateway now sets
   `ffi_limits.max_payload_bytes: 16 MiB` on its sql plugin and
   `max_request_body_mb: 32` (both chart-tunable: `state.ffiMaxPayloadBytes`,
   `state.maxRequestBodyMb`). So a single `state.get` / `state.admin.snapshot`
   returns a value or page up to 16 MiB in ONE call — no chunking — and agentd's
   own single-frame boot `state.get` restores any checkpoint up to that size.
   Verified: a 6 MiB value returns in one `state.get` with no error.

2. **Chunk beyond the cap (unbounded fallback).** For anything larger than
   whatever cap is configured, the control plane still has no hard limit:
   - **keyset pagination** — `state.admin.snapshot`/`state.admin.list` take
     `after`/`limit` and return `next`; a walk pages a whole prefix. `list` is
     keys-only (`{key, seq}`), used for existence checks and `migrate`'s compare.
   - **chunked read** — `state.admin.read_chunk {key, offset, length}` returns
     `substring(state::text …)` + total `len`; concatenated ordered slices
     reproduce the value byte-for-byte (md5-verified), then parse as JSON.
   - **chunked write** — `state.admin.write_chunk` stages ordered slices as rows
     (O(n) assembly), `state.admin.write_commit` string_aggs + UPSERTs them (a
     `agent_state_staging` table); so a value larger than the request body is
     restored.
   - `agentctl backup`'s `snapshot_all` is ADAPTIVE: batch via single-call
     snapshot, halve `limit` on a cap-hit, and stream a single oversized value
     via `read_chunked`. `restore` UPSERTs in batches and chunk-writes any item
     over the body. The apiserver's own ingress `DefaultBodyLimit` is lifted to
     512 MiB (only the mTLS-gated front proxy reaches it) so a large backup body
     is accepted. Errors surface (`result.isError`) instead of reading empty.

Verified live on `backend-sql:protocol-1`: a **6 MiB** value round-trips
`backup → restore` byte-intact via chunked write (staged in slices) even though
it exceeds the request body; a single call reads it back once the cap is raised;
a 552 KiB / 400-key mixed prefix backs up completely; the `state-durability` e2e
round-trips a ~400 KiB value through both `state.get` and `read_chunk`, and a
value through `write_chunk`/`write_commit`. There is no per-value or per-prefix
read-back size limit.

**Growth — bounded (was the last residual).** The size ceiling is gone; growth
is now bounded too. agentd writes one `run/<workflow>-<runid>` key per loop tick
and its own default retention is unbounded, so a managed prefix used to grow with
uptime. agentd confirmed (and we live-verified on 1.3.1) that
`store.retention.runs.keep_last` evicts terminal runs; the operator now renders
it on every managed agent (default 1000, chart `operator.runRetentionKeepLast`,
`0` disables), so steady-state size tracks retained + concurrent runs, not
uptime. That closes PLAN U10(a).

**Data-plane follow-ups (agentd, non-blocking).** (b) agentd is adding a
write-side bound `store.max_value_bytes` (refuse a checkpoint it couldn't read
back in one frame) — a backstop that at a 16 MiB cap essentially never fires; we
render it a few percent under the gateway `ffi_limits` cap once it ships, until
then unset. And a drift-proof future: mcpg advertising its effective result cap
on `initialize` (queued with mcpg) so agentd defaults `max_value_bytes` from it
instead of an operator keeping two numbers in sync.
