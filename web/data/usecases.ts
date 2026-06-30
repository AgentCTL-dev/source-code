import {
  BrainCircuitIcon,
  ContainerIcon,
  ShieldHalfIcon,
  WaypointsIcon,
  WorkflowIcon,
  type LucideIcon,
} from "lucide-react";

export type UseCase = {
  title: string;
  body: string;
  points: string[];
  icon: LucideIcon;
};

export const USE_CASES: UseCase[] = [
  {
    title: "Elastic agent workers",
    body: "Run a reactive worker pool that wakes on a backlog and drains back to zero — paying for nothing while idle.",
    icon: WorkflowIcon,
    points: [
      "claim-mode AgentFleet, KEDA scales 0→N→0",
      "atomic single-grant: one winner per item",
      "lease expiry re-offers work on a crash",
    ],
  },
  {
    title: "Secretless intelligence",
    body: "Give untrusted agent code model access without ever handing it a provider key or a network path of its own.",
    icon: BrainCircuitIcon,
    points: [
      "ModelGateway injects the key from a ModelPool",
      "per-pool token budgets, metered every call",
      "networkless agents infer over the substrate",
    ],
  },
  {
    title: "Multi-tenant agent platform",
    body: "Host many tenants' agents on shared infrastructure under a hostile-by-default trust model.",
    icon: ShieldHalfIcon,
    points: [
      "Kata-hybrid substrate for hard isolation",
      "attested identities + cross-tenant claim blocks",
      "NetworkPolicies, OIDC, SAR-gated verbs",
    ],
  },
  {
    title: "Agent-to-agent ecosystems",
    body: "Let agents discover and call each other across teams and orgs over a verifiable A2A mesh.",
    icon: WaypointsIcon,
    points: [
      "JWS-signed Agent Cards for discovery",
      "message/send and streaming message/stream",
      "OIDC or trusted-proxy edge authz",
    ],
  },
  {
    title: "Bring your own agent",
    body: "Any agent that conforms to the Agent Control Contract runs unchanged — agentctl depends on the contract, never a vendor.",
    icon: ContainerIcon,
    points: [
      "P0: depend on the contract, not an agent",
      "capabilities manifest + management profile",
      "agentd is the reference, not a dependency",
    ],
  },
];
