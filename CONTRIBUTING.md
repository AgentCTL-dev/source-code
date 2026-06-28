# Contributing to agentctl

Thanks for contributing! A few things keep this project healthy and its
licensing model viable.

## Licensing & the CLA (required)

agentctl is **dual-licensed by component** (see [`LICENSE`](LICENSE)): the
contract + SDK + tooling are **Apache-2.0** (open source); the runnable control
plane is **BUSL-1.1** (source-available, converts to Apache-2.0 on the Change
Date). To keep that model legal, the project must hold sufficient rights in every
contribution, so **all contributors must sign the [CLA](CLA.md)**.

You don't sign anything up front: when you open your first pull request, the **CLA
bot** comments with a one-line phrase to reply with. Once you've replied, the bot
records your signature and the check goes green; future PRs are automatic.

Please also **sign off your commits** (DCO) as a provenance record:

```sh
git commit -s -m "your message"   # adds a Signed-off-by: line
```

## Source headers

Every source file carries an SPDX header on line 1. New files **must** include it,
matching the crate's license (see the map in [`LICENSE`](LICENSE)):

```rust
// SPDX-License-Identifier: BUSL-1.1     // control-plane crates
// SPDX-License-Identifier: Apache-2.0   // contract / SDK / CLI / tooling
```

## P0 — depend on the contract, never on a specific agent

agentctl drives **any** agent conforming to the Agent Control Contract
(`contract/`, see `contract/SPEC.md`). Do not add a dependency on a specific agent
implementation (e.g. agentd) or branch on an agent's internals — code against the
contract. New wire shapes belong in the contract + the generated client, not
inline in a component.

## Dev workflow

```sh
cargo build --workspace
cargo test  --workspace
cargo clippy --workspace --all-targets -- -D warnings   # must be clean
cargo fmt --all                                          # must be clean
cargo run -p agentctl-crdgen                             # regenerate deploy/crds (no drift)
```

CI gates on all of the above (fmt, clippy `-D`, tests, CRD drift) — run them
locally first. End-to-end work is verified on a `kind` cluster; see
`deploy/README.md` and `docs/STATUS.md`.

## Where things live

- `README.md` — architecture + the 11 crates.
- `docs/STATUS.md` — per-plane status + roadmap.
- `rfcs/` — the design track (0001–0018).
- `contract/` — the Agent Control Contract (`README.md`, `SPEC.md`, schemas, fixtures).

By submitting a contribution you agree to the [CLA](CLA.md).
