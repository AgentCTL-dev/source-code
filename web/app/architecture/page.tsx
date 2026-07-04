import type { Metadata } from "next";
import { DocShell, H2, P, Ul, C, Note } from "@/components/site/doc-shell";
import { CodeBlock } from "@/components/site/code-block";
import { REPO_DOCS } from "@/data/site";

export const metadata: Metadata = {
  title: "Architecture",
  description:
    "How the agentctl control plane and the data-plane agents connect: agents serve mTLS HTTPS and dial the gateways keyless; identity is the boundary.",
};

export default function Page() {
  return (
    <DocShell
      title="Architecture"
      lead="How the control-plane components and the data-plane agents connect — agents are ordinary pods reached over the network, and identity is the boundary."
      editHref={`${REPO_DOCS}/architecture.md`}
    >
      <H2>The two directions</H2>
      <P>
        Everything reduces to two flows with two identity mechanisms. The control plane reaches{" "}
        <em>into</em> an agent over mTLS; the agent reaches <em>out</em> to the gateways keyless.
      </P>
      <CodeBlock
        lang="topology"
        code={`  kubectl / operator ─┐
  A2A client ─────────┤  APIServer · A2A gateway ──mTLS client cert──▶ agent :8443 /mcp
                      │  ModelGateway · MCPGateway ◀──source-IP attest── agent (keyless)
  control plane ──────┘
                          agent pod: serves mTLS /mcp · dials keyless
                                     no pod credential · restricted-PSS · no hostPath`}
      />
      <Ul>
        <li>
          <strong className="text-foreground">Into the agent (Management).</strong> The APIServer
          and A2A gateway dial <C>https://&lt;podIP&gt;:8443/mcp</C> presenting the control-plane
          client cert. A cert verified against the pinned client CA is <C>Management</C>; an
          unauthenticated request is refused, never downgraded.
        </li>
        <li>
          <strong className="text-foreground">Out of the agent (keyless).</strong> The agent dials{" "}
          <C>INTELLIGENCE</C> and each <C>--mcp</C> endpoint without any credential. The
          gateway attests the caller by source IP, injects the credential it holds off-pod, meters,
          and forwards.
        </li>
      </Ul>

      <H2>Provisioning &amp; PKI</H2>
      <P>
        The operator renders each agent pod to serve mTLS HTTPS and mints its identity through
        cert-manager: a cluster CA <C>ClusterIssuer</C>, a per-workload serving{" "}
        <C>Certificate</C> (<C>&lt;name&gt;-serving-tls</C>), and a per-namespace{" "}
        <C>agentctl-ca</C> ConfigMap so the agent trusts the gateways&apos; certs (<C>--tls-ca</C>).
        The control plane holds one client cert that mints Management at the agent&apos;s{" "}
        <C>/mcp</C>.
      </P>
      <P>
        The rendered pod is restricted-PSS — <C>runAsNonRoot</C>, drop <C>ALL</C> caps,{" "}
        <C>readOnlyRootFilesystem</C>, <C>automountServiceAccountToken: false</C>, no hostPath — and
        carries zero credentials (only a rotatable serving key). cert-manager rotates everything;
        agentd hot-reloads its serving cert with no restart.
      </P>

      <H2>Management</H2>
      <P>
        The aggregated APIServer exposes <C>drain</C> / <C>lame-duck</C> / <C>cancel</C> /{" "}
        <C>pause</C> / <C>resume</C> as SAR-gated verbs. Each resolves the Agent to its{" "}
        <C>status.podIP</C> and issues an <C>a2a.*</C> admin JSON-RPC call direct to the pod under
        the control-plane client cert. Per-verb RBAC and end-user identity survive the aggregation
        seam; there is no <C>pods/proxy</C>, no per-node agent, no host socket.
      </P>

      <H2>Intelligence &amp; tools</H2>
      <P>
        The ModelGateway is a standalone TLS Deployment the agent dials keyless. It attests the
        caller by source IP (a kube pod lookup, with a bounded cold-start retry), injects the
        ModelPool credential, meters per-pool tokens, and enforces the budget (a 429 on
        exhaustion). The MCPGateway is the tool-plane analog: an <C>MCPServerSet</C> binds servers,
        and the gateway attests, scopes the request to what the agent may reach, injects the
        server credential, and forwards. The pod holds no provider key and no tool credential.
      </P>

      <H2>A2A</H2>
      <P>
        The A2A gateway forwards direct to the agent pod <C>/mcp</C> on the contract&apos;s A2A wire —
        bare PascalCase methods (<C>SendMessage</C>, <C>GetTask</C>, …), the <C>{`{"task"}`}</C>{" "}
        envelope, and SSE streaming terminated by the terminal task state (no <C>final</C> flag). It
        builds the signed Agent Card from <C>agent://capabilities</C> and holds the durable task
        store; push config and version negotiation stay gateway-owned.
      </P>

      <Note>
        There is no per-node agent and nothing privileged on the host. Management, telemetry,
        inference, A2A, and MCP are each a network-native call between ordinary pods over mTLS.
      </Note>
    </DocShell>
  );
}
