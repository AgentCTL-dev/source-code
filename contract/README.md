# contract/ — the vendored Agent Control Contract (ACC 2)

The wire-and-process contract agentctl drives agents through, baselined on
**agentd 1.3.1**. `SPEC.md` is the normative text; `VERSION` pins the four
clocks; `schemas/` holds the per-plane artifacts; `fixtures/` holds real
captures from the pinned binary.

Regenerate the captured artifacts against a new agentd (then re-run
conformance and review the diff — a clock major bump is a breaking review):

```console
$ A=/path/to/agentd            # the pinned release binary
$ $A --config-schema   > schemas/config.schema.json
$ $A --workflow-schema > schemas/workflow.schema.json
$ $A --capabilities    > fixtures/capabilities/agentd-<ver>-default.json
$ (cd fixtures/config && HOOK_BEARER=x $A -c full-featured.yml --capabilities) \
                       > fixtures/capabilities/agentd-<ver>-configured.json
```

Hand-maintained (change only with an upstream source diff in hand, citing the
agentd file): `exit-codes.table.json` (`exit.rs`), `restart-only.json`
(`config/v2/mod.rs RESTART_ONLY_PATHS`), `env-convention.json`
(`ENV_ALIASES`), `metrics.registry.json` (`obs/metrics.rs`),
`a2a.profile.json`, `store.profile.json` (`store/mcp.rs default_ops`).

Consumed by `crates/agent-contract-client` (typed access + skew policy),
the operator's renderer and diff classifier, admission's binary-validation
step, and agentctl-e2e conformance.

Retired with ACC 1.x: `management-profile.json` (no served-MCP management
surface exists), `a2a.methods.json` (→ `a2a.profile.json`),
`events.schema.json`, `report.schema.json` (report/event shapes are no longer
contract anchors), and the `surfaces{}`-era capability fixtures.
