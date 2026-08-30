// SPDX-License-Identifier: Apache-2.0
//! # Registry resolution (RFC 0032 §2, P2-2)
//!
//! The pure scope-chain resolver every provisioning consumer calls (the
//! projection compiler, admission's policy ladder, the control MCP). Two
//! composition laws over `system → org → group → agent`:
//!
//! - **Content** (defaults, bundles, service grants): lower scopes ADD or
//!   SHADOW BY NAME — the most specific scope wins a field.
//! - **Security floors** (capability legs, egress closure, budget ceilings,
//!   tool ceilings, approval): lower scopes NARROW ONLY. A lower scope
//!   declaring a wider floor, or an agent requesting past the effective
//!   floor, is a [`Violation`] NAMING the scope whose floor was violated —
//!   never a silent clamp. This mirrors agentd's own catalog-tags-as-floors
//!   semantics, so the model holds end-to-end.
//!
//! Pure and total: no I/O, no clock; callers fetch the chain and hand it in.

use std::collections::BTreeMap;

use crate::v1alpha2::{
    AgentClassSpec, AgentSpec, Approval, Budget, Floors, Intelligence, MCPServiceSpec, Priority,
    RuntimeSelector, ServiceGrant, StoreSpec,
};

/// One link of the scope chain, most general FIRST (system → org → group).
pub struct ScopedClass<'a> {
    /// Human-readable scope label for violation messages
    /// (`"system class \"default\""`, `"org class \"acme-defaults\""`).
    pub scope: String,
    pub class: &'a AgentClassSpec,
}

/// A floor violation: what was asked, and WHOSE floor refused it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    /// The scope whose floor is violated.
    pub floor_scope: String,
    /// The defect, phrased for an admission denial.
    pub message: String,
}

impl std::fmt::Display for Violation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} refuses: {}", self.floor_scope, self.message)
    }
}

/// The resolved provisioning policy for one agent.
#[derive(Debug, Default)]
pub struct Resolved {
    pub runtime: Option<RuntimeSelector>,
    pub intelligence: Option<Intelligence>,
    pub store: Option<StoreSpec>,
    pub priority: Option<Priority>,
    pub approval: Option<Approval>,
    pub budget: Option<Budget>,
    /// Effective service grants: name → the NARROWED allow list (grant ∩
    /// registry ceiling). Every granted service resolved or it's an error.
    pub services: BTreeMap<String, ResolvedService>,
    /// Bundle refs, most-specific-last (shadowed by name).
    pub workflow_sets: Vec<String>,
    pub skill_sets: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedService {
    /// The effective tool allow list (empty = the registry entry's full
    /// ceiling).
    pub allow: Vec<String>,
    /// The entry's capability tags — UNCONDITIONAL floors the agent inherits.
    pub tags: Vec<String>,
}

/// Resolve the chain for one agent. `services` is the registry slice visible
/// at this scope (name → spec). Violations are collected exhaustively (an
/// admission denial should name every defect at once, not one per retry).
pub fn resolve(
    chain: &[ScopedClass<'_>],
    agent: &AgentSpec,
    services: &BTreeMap<String, MCPServiceSpec>,
) -> Result<Resolved, Vec<Violation>> {
    let mut violations = Vec::new();

    // -- floors fold: each link may only narrow its predecessor ------------
    let mut effective: Option<(String, Floors)> = None;
    for link in chain {
        if let Some(f) = &link.class.floors {
            match &effective {
                None => effective = Some((link.scope.clone(), f.clone())),
                Some((holder, current)) => {
                    let narrowed = narrow_floors(holder, current, &link.scope, f, &mut violations);
                    effective = Some(narrowed);
                }
            }
        }
    }

    // -- content fold: most specific wins -----------------------------------
    let mut resolved = Resolved::default();
    for link in chain {
        if let Some(d) = &link.class.defaults {
            if d.runtime.is_some() {
                resolved.runtime = d.runtime.clone();
            }
            if d.intelligence.is_some() {
                resolved.intelligence = d.intelligence.clone();
            }
            if d.store.is_some() {
                resolved.store = d.store.clone();
            }
            if d.priority.is_some() {
                resolved.priority = d.priority;
            }
            if d.approval.is_some() {
                resolved.approval = d.approval.clone();
            }
            if d.budget.is_some() {
                resolved.budget = d.budget.clone();
            }
        }
        for set in &link.class.workflow_sets {
            if !resolved.workflow_sets.contains(set) {
                resolved.workflow_sets.push(set.clone());
            }
        }
        for set in &link.class.skill_sets {
            if !resolved.skill_sets.contains(set) {
                resolved.skill_sets.push(set.clone());
            }
        }
    }
    // Agent-spec content wins over every class.
    if agent.runtime.is_some() {
        resolved.runtime = agent.runtime.clone();
    }
    if agent.intelligence.is_some() {
        resolved.intelligence = agent.intelligence.clone();
    }
    if agent.store.is_some() {
        resolved.store = agent.store.clone();
    }
    if agent.priority.is_some() {
        resolved.priority = agent.priority;
    }
    if agent.approval.is_some() {
        resolved.approval = agent.approval.clone();
    }
    if let Some(b) = agent.intelligence.as_ref().and_then(|i| i.budget.as_ref()) {
        resolved.budget = Some(b.clone());
    }
    for set in agent.skills.iter().map(|b| &b.set_ref) {
        if !resolved.skill_sets.contains(set) {
            resolved.skill_sets.push(set.clone());
        }
    }

    // -- service grants: class grants ∪ agent grants, ceilinged -------------
    let mut granted: BTreeMap<String, ServiceGrant> = BTreeMap::new();
    for link in chain {
        for g in &link.class.services {
            granted.insert(g.name.clone(), g.clone());
        }
    }
    for g in &agent.services {
        // The agent may NARROW a class grant (or introduce one the registry
        // slice permits) — its allow replaces the class grant's allow, and is
        // checked against the registry ceiling below.
        granted.insert(g.name.clone(), g.clone());
    }
    for (name, grant) in &granted {
        let Some(entry) = services.get(name) else {
            violations.push(Violation {
                floor_scope: "registry".into(),
                message: format!("service {name:?} is not in the visible registry slice"),
            });
            continue;
        };
        // grant allow ⊆ registry allow (empty registry allow = everything the
        // service serves; empty grant allow = the full registry ceiling).
        if !entry.allow.is_empty() {
            for pat in &grant.allow {
                if !pattern_within(pat, &entry.allow) {
                    violations.push(Violation {
                        floor_scope: format!("MCPService {name:?}"),
                        message: format!(
                            "allow pattern {pat:?} widens the registry ceiling {:?}",
                            entry.allow
                        ),
                    });
                }
            }
        }
        resolved.services.insert(
            name.clone(),
            ResolvedService {
                allow: if grant.allow.is_empty() {
                    entry.allow.clone()
                } else {
                    grant.allow.clone()
                },
                tags: entry.tags.clone(),
            },
        );
    }

    // -- agent requests vs the effective floors ------------------------------
    if let Some((holder, floors)) = &effective {
        check_agent_against_floors(holder, floors, agent, &resolved, &mut violations);
    }

    if violations.is_empty() {
        Ok(resolved)
    } else {
        Err(violations)
    }
}

/// Fold one lower-scope floor onto the current effective floor: the result is
/// the INTERSECTION, and any attempt to widen is a violation naming the
/// holder of the narrower floor.
fn narrow_floors(
    holder: &str,
    current: &Floors,
    lower_scope: &str,
    lower: &Floors,
    violations: &mut Vec<Violation>,
) -> (String, Floors) {
    let mut out = current.clone();

    // Capability legs: a leg the current floor denies cannot be re-allowed.
    // (`secrets` is a name list — holding ANY name is holding the leg.)
    if let Some(lower_caps) = &lower.capabilities {
        let cur = current.capabilities.clone().unwrap_or_default();
        let has_secrets = |c: &Option<Vec<String>>| c.as_ref().is_some_and(|v| !v.is_empty());
        let mut folded = cur.clone();
        for (leg, cur_held, low_held) in [
            (
                "exec",
                cur.exec.unwrap_or(false),
                lower_caps.exec.unwrap_or(false),
            ),
            (
                "egress",
                cur.egress.unwrap_or(false),
                lower_caps.egress.unwrap_or(false),
            ),
            (
                "secrets",
                has_secrets(&cur.secrets),
                has_secrets(&lower_caps.secrets),
            ),
        ] {
            if low_held && !cur_held && current.capabilities.is_some() {
                violations.push(Violation {
                    floor_scope: holder.to_string(),
                    message: format!(
                        "{lower_scope} tries to re-allow the {leg:?} capability leg the floor denies"
                    ),
                });
            }
            let folded_held = cur_held && low_held;
            match leg {
                "exec" => folded.exec = Some(folded_held),
                "egress" => folded.egress = Some(folded_held),
                _ => {
                    if !folded_held {
                        folded.secrets = None;
                    }
                }
            }
        }
        if current.capabilities.is_some() {
            out.capabilities = Some(folded);
        } else {
            out.capabilities = Some(lower_caps.clone());
        }
    }

    // Egress closure: `closed` is narrower than open; reopening is refused.
    match (current.egress.as_deref(), lower.egress.as_deref()) {
        (Some("closed"), Some(l)) if l != "closed" => violations.push(Violation {
            floor_scope: holder.to_string(),
            message: format!("{lower_scope} tries to reopen egress ({l:?}) past the closed floor"),
        }),
        (_, Some(_)) if current.egress.is_none() => out.egress = lower.egress.clone(),
        _ => {}
    }

    // Budget ceiling: only shrink.
    if let Some(lower_budget) = &lower.budget {
        match &current.budget {
            None => out.budget = Some(lower_budget.clone()),
            Some(cur) => {
                if budget_exceeds(lower_budget, cur) {
                    violations.push(Violation {
                        floor_scope: holder.to_string(),
                        message: format!(
                            "{lower_scope} raises the budget ceiling past the floor ({})",
                            budget_brief(cur)
                        ),
                    });
                } else {
                    out.budget = Some(lower_budget.clone());
                }
            }
        }
    }

    // Tool ceiling: lower patterns must fit inside the current ones.
    if !lower.tools.is_empty() {
        if current.tools.is_empty() {
            out.tools = lower.tools.clone();
        } else {
            for pat in &lower.tools {
                if !pattern_within(pat, &current.tools) {
                    violations.push(Violation {
                        floor_scope: holder.to_string(),
                        message: format!(
                            "{lower_scope} tool pattern {pat:?} widens the ceiling {:?}",
                            current.tools
                        ),
                    });
                }
            }
            out.tools = lower.tools.clone();
        }
    }

    // Approval: ask > auto (deny strongest). Weakening is refused.
    if let Some(low_app) = &lower.approval {
        let rank = |a: &str| match a {
            "deny" => 2,
            "ask" => 1,
            _ => 0,
        };
        match &current.approval {
            Some(cur) if rank(low_app) < rank(cur) => violations.push(Violation {
                floor_scope: holder.to_string(),
                message: format!("{lower_scope} weakens the approval floor {cur:?} to {low_app:?}"),
            }),
            _ => out.approval = Some(low_app.clone()),
        }
    }

    (holder.to_string(), out)
}

/// The agent's own requests against the effective floors.
fn check_agent_against_floors(
    holder: &str,
    floors: &Floors,
    agent: &AgentSpec,
    resolved: &Resolved,
    violations: &mut Vec<Violation>,
) {
    if let (Some(req), Some(cap_floor)) = (&agent.capabilities, &floors.capabilities) {
        let has_secrets = |c: &Option<Vec<String>>| c.as_ref().is_some_and(|v| !v.is_empty());
        for (leg, r, f) in [
            (
                "exec",
                req.exec.unwrap_or(false),
                cap_floor.exec.unwrap_or(false),
            ),
            (
                "egress",
                req.egress.unwrap_or(false),
                cap_floor.egress.unwrap_or(false),
            ),
            (
                "secrets",
                has_secrets(&req.secrets),
                has_secrets(&cap_floor.secrets),
            ),
        ] {
            if r && !f {
                violations.push(Violation {
                    floor_scope: holder.to_string(),
                    message: format!(
                        "the agent requests the {leg:?} capability leg the floor denies"
                    ),
                });
            }
        }
    }

    if floors.egress.as_deref() == Some("closed") {
        // Closed egress: raw egress requests must come through tagged grants.
        let has_egress_service = resolved
            .services
            .values()
            .any(|s| s.tags.iter().any(|t| t == "egress"));
        let requests_egress = agent
            .capabilities
            .as_ref()
            .and_then(|c| c.egress)
            .unwrap_or(false);
        if requests_egress && !has_egress_service {
            violations.push(Violation {
                floor_scope: holder.to_string(),
                message: "egress is closed: raw capabilities.egress needs an egress-tagged \
                          service grant"
                    .into(),
            });
        }
        if !agent.mcp_servers.is_empty() {
            violations.push(Violation {
                floor_scope: holder.to_string(),
                message: "egress is closed: inline spec.mcpServers bypass the service registry \
                          (register an MCPService and grant it)"
                    .into(),
            });
        }
    }

    if let Some(ceiling) = &floors.budget {
        if let Some(b) = &resolved.budget {
            if budget_exceeds(b, ceiling) {
                violations.push(Violation {
                    floor_scope: holder.to_string(),
                    message: format!(
                        "the requested budget exceeds the ceiling ({})",
                        budget_brief(ceiling)
                    ),
                });
            }
        }
    }

    if !floors.tools.is_empty() {
        for svc in resolved.services.values() {
            for pat in &svc.allow {
                if !pattern_within(pat, &floors.tools) {
                    violations.push(Violation {
                        floor_scope: holder.to_string(),
                        message: format!(
                            "tool allow {pat:?} is outside the tool ceiling {:?}",
                            floors.tools
                        ),
                    });
                }
            }
        }
    }
}

/// Does `pat` fit within `ceiling`? Conservative implication: `pat` fits when
/// it equals a ceiling pattern, or when it is a LITERAL (no `*`) matching a
/// ceiling glob. A glob request against a different glob ceiling does NOT fit
/// (glob-implies-glob is undecidable cheaply; narrow-only means the burden is
/// on the requester to be concrete).
fn pattern_within(pat: &str, ceiling: &[String]) -> bool {
    ceiling
        .iter()
        .any(|c| c == pat || (!pat.contains('*') && crate::org::access::glob_match(c, pat)))
}

/// Does `b` exceed `ceiling` anywhere? Lifetime and per-window (a window the
/// ceiling bounds must exist in `b` no larger; a ceiling window absent from
/// `b` is fine — absence spends nothing… but an UNBOUNDED b against a bounded
/// ceiling exceeds it).
fn budget_exceeds(b: &Budget, ceiling: &Budget) -> bool {
    if let Some(cl) = ceiling.lifetime_tokens {
        match b.lifetime_tokens {
            Some(bl) if bl <= cl => {}
            _ => return true, // larger, or unbounded
        }
    }
    for cw in &ceiling.windows {
        match b.windows.iter().find(|w| w.per == cw.per) {
            Some(bw) if bw.tokens <= cw.tokens => {}
            _ => return true,
        }
    }
    false
}

fn budget_brief(b: &Budget) -> String {
    let mut parts = Vec::new();
    if let Some(l) = b.lifetime_tokens {
        parts.push(format!("lifetime {l}"));
    }
    for w in &b.windows {
        parts.push(format!("{}/{}", w.tokens, w.per));
    }
    parts.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::v1alpha2::{BudgetWindow, ClassDefaults};

    fn class(yaml: &str) -> AgentClassSpec {
        serde_yaml::from_str(yaml).unwrap()
    }
    fn agent(yaml: &str) -> AgentSpec {
        serde_yaml::from_str(yaml).unwrap()
    }
    fn services(pairs: &[(&str, &str)]) -> BTreeMap<String, MCPServiceSpec> {
        pairs
            .iter()
            .map(|(n, y)| (n.to_string(), serde_yaml::from_str(y).unwrap()))
            .collect()
    }

    #[test]
    fn content_shadows_by_name_and_agent_wins() {
        let system = class("defaults: { runtime: { version: \"1.3.0\" }, priority: low }");
        let org = class("defaults: { runtime: { version: \"1.3.1\" } }");
        let chain = [
            ScopedClass {
                scope: "system class \"default\"".into(),
                class: &system,
            },
            ScopedClass {
                scope: "org class \"acme\"".into(),
                class: &org,
            },
        ];
        let a = agent("shape: job\ntriggers: [{ once: {} }]\npriority: high");
        let r = resolve(&chain, &a, &BTreeMap::new()).unwrap();
        // org shadowed system's runtime; system's priority survived until the
        // agent's own request won.
        assert_eq!(r.runtime.unwrap().version.as_deref(), Some("1.3.1"));
        assert_eq!(r.priority, Some(Priority::High));
    }

    #[test]
    fn widening_a_floor_is_refused_naming_the_holder() {
        let org = class(
            "floors: { egress: closed, budget: { windows: [{ per: day, tokens: 100000 }] } }",
        );
        let group =
            class("floors: { egress: open, budget: { windows: [{ per: day, tokens: 900000 }] } }");
        let chain = [
            ScopedClass {
                scope: "org class \"acme\"".into(),
                class: &org,
            },
            ScopedClass {
                scope: "group class \"eng\"".into(),
                class: &group,
            },
        ];
        let a = agent("shape: job\ntriggers: [{ once: {} }]");
        let errs = resolve(&chain, &a, &BTreeMap::new()).unwrap_err();
        let text: Vec<String> = errs.iter().map(|v| v.to_string()).collect();
        assert!(
            text.iter()
                .any(|t| t.contains("org class \"acme\"") && t.contains("reopen egress")),
            "{text:?}"
        );
        assert!(
            text.iter().any(|t| t.contains("raises the budget ceiling")),
            "{text:?}"
        );
    }

    #[test]
    fn narrowing_is_welcome_and_becomes_the_effective_floor() {
        let org = class("floors: { budget: { windows: [{ per: day, tokens: 100000 }] } }");
        let group = class("floors: { budget: { windows: [{ per: day, tokens: 50000 }] } }");
        let chain = [
            ScopedClass {
                scope: "org".into(),
                class: &org,
            },
            ScopedClass {
                scope: "group".into(),
                class: &group,
            },
        ];
        // The agent asks past the GROUP floor but under the org one → refused
        // (the effective floor is the narrowest).
        let a = agent(
            "shape: job\ntriggers: [{ once: {} }]\nintelligence: { budget: { windows: [{ per: day, tokens: 80000 }] } }",
        );
        let errs = resolve(&chain, &a, &BTreeMap::new()).unwrap_err();
        assert!(
            errs[0].message.contains("exceeds the ceiling (50000/day)"),
            "{errs:?}"
        );

        let ok = agent(
            "shape: job\ntriggers: [{ once: {} }]\nintelligence: { budget: { windows: [{ per: day, tokens: 40000 }] } }",
        );
        assert!(resolve(&chain, &ok, &BTreeMap::new()).is_ok());
    }

    #[test]
    fn service_grants_ceiling_and_closed_egress() {
        let org = class("floors: { egress: closed }\nservices: [{ name: zendesk }]");
        let chain = [ScopedClass {
            scope: "org class \"acme\"".into(),
            class: &org,
        }];
        let registry = services(&[(
            "zendesk",
            "kind: mcp\nendpoint: https://z/mcp\ntags: [egress]\nallow: [\"ticket_*\"]",
        )]);

        // Narrowing the grant is fine; the resolved allow is the agent's.
        let a = agent(
            "shape: daemon\nexpose: { a2a: true }\nservices: [{ name: zendesk, allow: [ticket_read] }]\ncapabilities: { egress: true }",
        );
        let r = resolve(&chain, &a, &registry).unwrap();
        assert_eq!(r.services["zendesk"].allow, vec!["ticket_read"]);
        assert_eq!(r.services["zendesk"].tags, vec!["egress"]);

        // Widening past the registry ceiling is refused naming the service.
        let wide = agent(
            "shape: daemon\nexpose: { a2a: true }\nservices: [{ name: zendesk, allow: [admin_wipe] }]",
        );
        let errs = resolve(&chain, &wide, &registry).unwrap_err();
        assert!(errs[0].floor_scope.contains("zendesk"), "{errs:?}");
        assert!(errs[0].message.contains("widens the registry ceiling"));

        // Closed egress: raw capability without ANY tagged grant is refused —
        // note the org class above grants zendesk (tagged egress), which
        // legitimately satisfies the closure; the refusal needs a chain with
        // no such grant.
        let bare_org = class("floors: { egress: closed }");
        let bare_chain = [ScopedClass {
            scope: "org class \"acme\"".into(),
            class: &bare_org,
        }];
        let raw = agent("shape: daemon\nexpose: { a2a: true }\ncapabilities: { egress: true }");
        let errs = resolve(&bare_chain, &raw, &registry).unwrap_err();
        assert!(
            errs[0].message.contains("egress-tagged service grant"),
            "{errs:?}"
        );

        // …and inline mcpServers bypassing the registry are refused.
        let inline = agent(
            "shape: daemon\nexpose: { a2a: true }\nmcpServers: [{ name: x, endpoint: \"https://x/mcp\" }]",
        );
        let errs = resolve(&chain, &inline, &registry).unwrap_err();
        assert!(
            errs[0].message.contains("bypass the service registry"),
            "{errs:?}"
        );
    }

    #[test]
    fn unknown_service_and_tool_ceiling() {
        let org = class("floors: { tools: [\"ticket_*\"] }");
        let chain = [ScopedClass {
            scope: "org".into(),
            class: &org,
        }];
        let registry = services(&[("zendesk", "kind: mcp\nendpoint: https://z/mcp")]);

        let a = agent(
            "shape: daemon\nexpose: { a2a: true }\nservices: [{ name: ghost }, { name: zendesk, allow: [wipe_all] }]",
        );
        let errs = resolve(&chain, &a, &registry).unwrap_err();
        let text: Vec<String> = errs.iter().map(|v| v.to_string()).collect();
        assert!(
            text.iter()
                .any(|t| t.contains("not in the visible registry")),
            "{text:?}"
        );
        assert!(
            text.iter().any(|t| t.contains("outside the tool ceiling")),
            "{text:?}"
        );

        // A literal inside the glob ceiling fits.
        let ok = agent(
            "shape: daemon\nexpose: { a2a: true }\nservices: [{ name: zendesk, allow: [ticket_read] }]",
        );
        assert!(resolve(&chain, &ok, &registry).is_ok());
    }

    #[test]
    fn budget_exceeds_treats_unbounded_as_exceeding() {
        let ceiling = Budget {
            lifetime_tokens: Some(1000),
            windows: vec![BudgetWindow {
                per: "day".into(),
                tokens: 100,
            }],
        };
        // No lifetime bound at all exceeds a bounded ceiling.
        assert!(budget_exceeds(
            &Budget {
                lifetime_tokens: None,
                windows: vec![]
            },
            &ceiling
        ));
        assert!(!budget_exceeds(
            &Budget {
                lifetime_tokens: Some(500),
                windows: vec![BudgetWindow {
                    per: "day".into(),
                    tokens: 100
                }]
            },
            &ceiling
        ));
    }

    #[test]
    fn defaults_type_checks() {
        // ClassDefaults round-trips (guards the serde shape).
        let d: ClassDefaults =
            serde_yaml::from_str("store: { class: managed }\napproval: { policy: ask }").unwrap();
        assert_eq!(d.approval.unwrap().policy.as_deref(), Some("ask"));
    }
}
