import type { Metadata } from "next";
import { DocShell, H2, P, Ul, C, Note } from "@/components/site/doc-shell";
import { CodeBlock } from "@/components/site/code-block";
import { GITHUB_URL, AGENTD_IMAGE } from "@/data/site";

export const metadata: Metadata = {
  title: "Install",
  description:
    "Install the agentctl control plane with Helm. cert-manager is the only hard prerequisite; Postgres is bundled; KEDA is optional (claim-mode autoscaling).",
};

export default function Page() {
  return (
    <DocShell
      title="Install"
      lead="Bring up the control plane with Helm, then apply an Agent CR. cert-manager is the only hard external prerequisite."
      editHref={`${GITHUB_URL}/tree/main/charts/agentctl`}
    >
      <H2>Prerequisites</H2>
      <Ul>
        <li>
          <strong className="text-foreground">cert-manager (≥ 1.13) — hard.</strong> Issues every
          serving / mTLS cert and injects the caBundles.
        </li>
        <li>
          <strong className="text-foreground">Postgres — bundled</strong> by the chart (eval), or
          external for prod (<C>postgres.mode=external</C>).
        </li>
        <li>
          <strong className="text-foreground">KEDA — optional</strong>, only for claim-mode
          autoscaling. The chart installs and runs fully without it.
        </li>
      </Ul>

      <H2>1 · cert-manager</H2>
      <CodeBlock
        lang="bash"
        code={`kubectl apply -f https://github.com/cert-manager/\\
  cert-manager/releases/latest/download/cert-manager.yaml`}
      />

      <H2>2 · the control plane</H2>
      <P>
        No control-plane component needs hostPath / hostPID / privilege — every component is an
        ordinary pod on the pod network — so the <C>baseline</C> PodSecurity level suffices.
      </P>
      <CodeBlock
        lang="bash"
        code={`kubectl create namespace agentctl-system
kubectl label  namespace agentctl-system \\
  pod-security.kubernetes.io/enforce=baseline

helm install agentctl ./charts/agentctl -n agentctl-system

kubectl -n agentctl-system get pods         # all Running
kubectl -n agentctl-system get certificate  # all READY=True
kubectl get apiservice v1alpha1.management.agents.x-k8s.io  # AVAILABLE`}
      />
      <Note>
        On upgrade use <C>helm upgrade --reset-then-reuse-values</C> — plain <C>--reuse-values</C>{" "}
        drops newly added value blocks.
      </Note>

      <H2>3 · run an agent</H2>
      <P>
        Declare an <C>Agent</C>. The operator renders a restricted-PSS pod that serves mTLS{" "}
        <C>:8443/mcp</C>, dials intelligence keyless, and mounts the per-namespace CA — no
        credential on the pod.
      </P>
      <CodeBlock
        lang="agent.yaml"
        code={`apiVersion: agents.x-k8s.io/v1alpha1
kind: Agent
metadata: { name: hello, namespace: team-a }
spec:
  image: ${AGENTD_IMAGE}
  mode: reactive
  # modelPool: gpt          # keyless intelligence (ModelGateway holds the key)
  # mcpServerSetRefs: [tools] # brokered tools (MCPGateway injects the credential)`}
      />
      <CodeBlock lang="bash" code={`kubectl apply -f agent.yaml
kubectl get agents -n team-a    # READY=True`} />

      <H2>Production notes</H2>
      <Ul>
        <li>
          <strong className="text-foreground">External Postgres:</strong>{" "}
          <C>--set postgres.mode=external --set postgres.external.dsnSecretName=my-pg</C>.
        </li>
        <li>
          <strong className="text-foreground">Private registry:</strong>{" "}
          <C>--set image.registry=ghcr.io/your-org --set image.tag=vX.Y.Z</C>.
        </li>
        <li>
          <strong className="text-foreground">Your own CA:</strong>{" "}
          <C>--set certManager.caIssuerRef=my-ca-clusterissuer</C> to chain into an existing PKI.
        </li>
      </Ul>
    </DocShell>
  );
}
