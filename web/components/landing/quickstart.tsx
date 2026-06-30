import { Reveal } from "@/components/motion/reveal";
import { SectionHeading } from "@/components/landing/section-heading";
import { highlight } from "@/lib/highlight";
import {
  QuickstartTabs,
  type QuickstartTab,
} from "@/components/landing/quickstart-tabs";

// All snippets are real: kind from README.md, Helm-from-GHCR from
// charts/agentctl/README.md, the Agent manifest from deploy/examples (with the
// real CRD group agents.x-k8s.io/v1alpha1 + the agentd reference image).
type RawTab = {
  value: string;
  label: string;
  blurb: string;
  blocks: { title: string; lang: string; code: string }[];
};

const RAW_TABS: RawTab[] = [
  {
    value: "kind",
    label: "kind (local)",
    blurb:
      "Spin up a local cluster, install the contract CRDs + control plane, and provision your first agent.",
    blocks: [
      {
        title: "quickstart.sh",
        lang: "bash",
        code: `kind create cluster --name agentctl

# build + load images, install CRDs + operator + node-agent + apiserver
cargo run -p agentctl-crdgen && kubectl apply -f deploy/crds/

kubectl apply -f deploy/examples/agent-once.yaml
kubectl get agents`,
      },
    ],
  },
  {
    value: "helm",
    label: "Helm from GHCR",
    blurb:
      "Install the published chart + component images straight from the GHCR registry — no local image loading.",
    blocks: [
      {
        title: "install.sh",
        lang: "bash",
        code: `# the node-agent needs a privileged PodSecurity namespace
kubectl create namespace agentctl-system
kubectl label  namespace agentctl-system \\
  pod-security.kubernetes.io/enforce=privileged \\
  pod-security.kubernetes.io/warn=privileged

helm install agentctl oci://ghcr.io/agentctl-dev/charts/agentctl \\
  -n agentctl-system --version 0.1.0 \\
  --set image.registry=ghcr.io/agentctl-dev --set image.tag=0.1.0`,
      },
      {
        title: "verify.sh",
        lang: "bash",
        code: `kubectl -n agentctl-system get pods                        # 7 components Running
kubectl -n agentctl-system get certificate                 # all READY=True
kubectl get apiservice v1alpha1.management.agents.x-k8s.io  # AVAILABLE=True`,
      },
    ],
  },
  {
    value: "agent",
    label: "Deploy an agentd Agent",
    blurb:
      "Declare a once-mode Agent backed by the agentd reference image; the operator renders it to a confined Job and patches Agent.status.",
    blocks: [
      {
        title: "agent.yaml",
        lang: "yaml",
        code: `apiVersion: agents.x-k8s.io/v1alpha1
kind: Agent
metadata:
  name: summarizer
  namespace: default
spec:
  mode: once
  image: ghcr.io/agentd-dev/agentd:1.0.0
  instruction: "Read /data/report.md and write a 3-bullet summary to /data/summary.md"`,
      },
      {
        title: "apply.sh",
        lang: "bash",
        code: `kubectl apply -f agent.yaml
kubectl get agents
kubectl describe agent summarizer`,
      },
    ],
  },
];

export async function Quickstart() {
  const tabs: QuickstartTab[] = await Promise.all(
    RAW_TABS.map(async (t) => ({
      value: t.value,
      label: t.label,
      blurb: t.blurb,
      blocks: await Promise.all(
        t.blocks.map(async (b) => ({
          title: b.title,
          raw: b.code,
          html: await highlight(b.code, b.lang),
        })),
      ),
    })),
  );

  return (
    <section id="quickstart" className="scroll-mt-20 border-b">
      <div className="mx-auto max-w-4xl px-4 py-20 sm:py-24">
        <Reveal>
          <SectionHeading
            eyebrow="Quickstart"
            title="From zero to a running agent"
            align="center"
          >
            Three real paths: a local kind cluster, the published Helm chart from
            GHCR, or a single Agent manifest. Every snippet is copy-paste ready.
          </SectionHeading>
        </Reveal>
        <Reveal delay={0.1} className="mt-10">
          <QuickstartTabs tabs={tabs} />
        </Reveal>
      </div>
    </section>
  );
}
