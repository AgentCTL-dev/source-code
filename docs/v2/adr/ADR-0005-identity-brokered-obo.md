# ADR-0005 — identity is brokered: external IdPs + a custody/exchange service

**Status:** Accepted · **Date:** 2026-08-30 · **Relates:** RFC 0028, ARCHITECTURE §3.4, §4

## Context

Requirements: work with Auth0/Keycloak/Okta and modern OAuth/OIDC AI extensions
(29); a standalone identity component that exchanges user/workload tokens OBO-style
so a days-old agent always has a fresh token for the next API/MCP in the chain
(17); orgs/groups/users (21); secrets stores (23). agentd contributes: AAuth
outbound signing with federated enrollment (a projected ServiceAccount token file,
re-read per enroll), `acting_for` attribution, per-principal quotas — but no
inbound verification. mcpg contributes: AAuth/OIDC/JWT/mTLS identity chains and
per-request credential issuers that can call an external exchanger.

## Decision

1. **agentctl never owns user accounts.** All human identity comes from external
   OIDC IdPs; agentctl maps claims → org/group/user. CLI uses device flow; web
   uses auth code + PKCE.
2. **`agentctl-identity` is a standalone service** with four duties:
   (a) OIDC federation + token validation for gateway/apiserver/mcpg;
   (b) **grant custody**: per-user per-provider connections (offline refresh
   tokens, obtained by explicit consent), envelope-encrypted (KMS pluggable) —
   the only place long-lived user credentials exist;
   (c) **exchange**: RFC 8693 token exchange (+ ID-JAG where supported) minting
   short-lived audience-scoped tokens for (agent identity, acting_for user,
   target); cached, proactively refreshed;
   (d) **agent identity**: the AAuth agent provider for the fleet (federated
   enrollment via projected SA tokens; JWKS for mcpg verification) and the mint
   for per-(user, agent) A2A principal bearers.
3. Agents (and supervisors) **never hold refresh tokens or long-lived user
   credentials** — only their own workload identity; user authority is always
   acquired per call via the exchange (through mcpg's `cred://`, or by the
   gateway's principal injection).

## Consequences

- Compromising an agent yields its own narrow identity, not the user's account;
  revocation is central (kill the connection/binding, all downstream minting
  stops).
- Long-running agents get fresh credentials indefinitely — item 16/17's core.
- IdP feature drift (which grants each IdP supports) is absorbed by identity's
  provider adapters, not by every consumer; ID-JAG/token-exchange support varies
  and is negotiated per provider.
- One more stateful service (Postgres + KMS) with hard security requirements —
  gets the strictest review, its own threat model in RFC 0028.

## Alternatives rejected

- Bundling an IdP (running our own Keycloak): heavy, duplicative; enterprises
  already have IdPs. (The chart may *optionally* install Keycloak for dev.)
- Doing exchange inside mcpg only (its BUSL token-exchange plugin): leaves custody
  and A2A principal minting homeless, and spreads vault material into every
  gateway; a single custody service with a thin Apache issuer plugin is cleaner.
