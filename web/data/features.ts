import {
  ActivityIcon,
  BoxesIcon,
  BrainCircuitIcon,
  ShieldCheckIcon,
  WaypointsIcon,
  WorkflowIcon,
  type LucideIcon,
} from "lucide-react";

export type Feature = {
  tag: string;
  title: string;
  body: string;
  icon: LucideIcon;
  href: string;
  linkLabel: string;
};

// The six planes of the control plane. Each links to the doc that currently
// documents that flow (architecture.md carries the per-flow sequence diagrams;
// security.md the auth model; operations.md day-2); repoint to the dedicated
// guides once the docs lane lands them.
export const FEATURES: Feature[] = [
  {
    tag: "provisioning",
    title: "Declarative agents",
    body: "Declare an Agent or AgentFleet; the operator reconciles it to a Job, Deployment, or StatefulSet on a stock-unix or Kata substrate — confined securityContext, downward-API env, management socket bound. Status reflects reality, with finalizer GC.",
    icon: BoxesIcon,
    href: "/docs/architecture",
    linkLabel: "Provisioning flow",
  },
  {
    tag: "scaling",
    title: "Elastic fleets",
    body: "Claim-mode worker pools scale 0→N→0 on backlog through a KEDA external scaler. Shard-mode StatefulSets give every replica a deterministic K/N slice (fnv1a64 of the shard key) so keyed work stays ordered.",
    icon: WorkflowIcon,
    href: "/docs/architecture",
    linkLabel: "Scaling flows",
  },
  {
    tag: "intelligence",
    title: "Secretless & budgeted",
    body: "Agents reach models through the ModelGateway: credentials injected at the edge from a ModelPool, every token metered, budgets enforced (over cap → HTTP 429). No provider key ever lands on a pod.",
    icon: BrainCircuitIcon,
    href: "/docs/security",
    linkLabel: "Intelligence path",
  },
  {
    tag: "a2a",
    title: "Agent-to-agent mesh",
    body: "A JWS-signed Agent Card and an A2A gateway broker message/send and SSE message/stream between agents — gated by per-agent OIDC or a trusted front-proxy at the edge, with caller identity forwarded.",
    icon: WaypointsIcon,
    href: "/docs/architecture",
    linkLabel: "A2A path",
  },
  {
    tag: "security",
    title: "Hostile multi-tenancy",
    body: "mTLS, SO_PEERCRED kernel attestation, per-agent OIDC, NetworkPolicies, and SubjectAccessReview-gated management verbs. Every capability is gated default-off; the agent runs networkless and secretless.",
    icon: ShieldCheckIcon,
    href: "/docs/security",
    linkLabel: "Security model",
  },
  {
    tag: "observability",
    title: "Everything is metered",
    body: "A frozen metric surface across every plane, a registered exit-code table, and reconcile-latency histograms — wired for Prometheus. The node-agent scrape-proxies networkless agents so they stay observable.",
    icon: ActivityIcon,
    href: "/docs/operations",
    linkLabel: "Observability",
  },
];
