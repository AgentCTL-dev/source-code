# ADR-0011 — one Helm umbrella chart installs everything

**Status:** Accepted · **Date:** 2026-08-30 · **Relates:** ARCHITECTURE §13; evolves charts/agentctl

## Context

Requirement 19/31: agentctl is infra orchestration, installed by Helm — apps plus
the wiring (CRDs, PKI, policies, data services, mcpg, dashboards). A chart already
exists (`charts/agentctl`) covering the v1 components, bundled Postgres, and
cert-manager integration.

## Decision

1. `charts/agentctl` becomes the **umbrella**: agentctl components (operator,
   apiserver, gateway, identity, admission, scaler), CRDs, PKI issuers, default
   NetworkPolicies, dashboards, system registry defaults — plus dependencies:
   **mcpg-operator** (+ system gateway instance), **Postgres** (bundled or
   external), optional object-store (external endpoint config; bundled MinIO for
   dev only), optional **Keycloak** (dev), optional **agentd-ui**.
2. **Profiles over knobs**: `profile: dev | production` value presets (HA replica
   counts, PDBs, anti-affinity, external data stores required in production);
   `values.schema.json` maintained; every image referenced by digest and
   cosign-verifiable.
3. **CRD lifecycle**: CRDs ship in the chart's `crds/` with a documented
   upgrade path (conversion webhooks per RFC 0005's mechanism, reused for
   v1alpha1→v1alpha2).
4. Air-gap: all images/charts mirrorable; mcpg plugin mirror CR supported; no
   install-time internet dependency.
5. GitOps-first: the chart is Argo/Flux-friendly (no install-order hooks that
   break sync waves; jobs idempotent).

## Consequences

- `helm install` + IdP values = a working agent cloud; day-2 is CRs.
- Umbrella version pins the compatibility matrix (agentctl × agentd × mcpg) —
  released and tested as one line item.
- Bundled-PG remains dev-grade; production requires external PG/S3 (the v1
  kind-disk-full lesson is encoded as a hard values gate in the production
  profile).

## Alternatives rejected

- Operator-installs-everything (OLM-style meta-operator): heavier and less
  transparent than Helm for this audience; revisit if OpenShift demand appears.
- Separate charts per component as the primary interface: pushes the
  compatibility matrix onto users; sub-charts remain available for experts.
