//! # agentctl-node-agent
//!
//! The on-node bridge (agentctl RFC 0008). This crate currently holds the
//! **Tier A management bridge** ([`mgmt`]): the client that speaks a conformant
//! agent's *management profile* (the self-MCP, RFC 0005/0015) over the
//! discovered substrate socket — the backend for `kubectl agent
//! describe/tree/drain` (RFC 0009/0016).
//!
//! The transport is the contract's management wire: **NDJSON JSON-RPC** over a
//! `UnixStream` (the stock-unix substrate, RFC 0002; the Kata-hybrid tier dials
//! a per-VM uds with the same code path). It is deliberately blocking and
//! dependency-light here; the async Tier-A/Tier-B split and CID/socket discovery
//! are layered on next.

pub mod discovery;
pub mod metrics;
pub mod mgmt;

pub use discovery::{discover, DiscoveredAgent};
pub use mgmt::{Error, ManagementClient};
