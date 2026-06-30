export type Stat = {
  // Either a count-up number (value) or a static display string (display).
  value?: number;
  display?: string;
  decimals?: number;
  prefix?: string;
  suffix?: string;
  label: string;
  detail: string;
};

// Real measured figures from the Phase-4 e2e + scale harness (see docs/benchmarks.md).
// Flip to `false` to hide the Benchmarks section.
export const BENCHMARKS_ENABLED = true;

export const BENCHMARKS: Stat[] = [
  {
    value: 5137,
    label: "work ops / sec",
    detail:
      "coordination throughput at 256 concurrent clients — p99 < 100 ms, 0 errors over 41k+ ops",
  },
  {
    value: 1.3,
    decimals: 1,
    suffix: "m",
    label: "CPU per agent",
    detail:
      "marginal cost of one idle agentd agent (millicores; sub-MiB working set) — the overhead floor",
  },
  {
    value: 0,
    label: "double-grants",
    detail: "atomic single-grant held under full contention (incl. across Postgres-backed replicas)",
  },
  {
    value: 100,
    prefix: "1→",
    label: "agents, flat control plane",
    detail: "reconcile p95 ~24 ms and ~65 MiB control-plane memory, unchanged from 1 to 100 agents",
  },
];

export const BENCHMARKS_CAVEAT =
  "Measured on a single-node kind cluster (16-vCPU AMD EPYC) driving the agentd v1.0.0 " +
  "reference agent (ghcr.io/agentd-dev/agentd:1.0.0, a ~1.3 MB static binary). Absolute " +
  "capacity is host-bound; the durable results are per-agent overhead + control-plane " +
  "trends from the re-runnable harness. Full report + methodology in docs/benchmarks.md; " +
  "re-run on a real multi-node cluster for production capacity numbers.";
