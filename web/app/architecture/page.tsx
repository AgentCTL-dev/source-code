import type { Metadata } from "next";
import { DocShell, H2, P, Ul, C, Note } from "@/components/site/doc-shell";
import { CodeBlock } from "@/components/site/code-block";
import { REPO_DOCS } from "@/data/site";

export const metadata: Metadata = {
  title: "Architecture",
  description:
    "How the agentctl control plane and the data-plane agents connect: agents serve mTLS HTTPS and dial their LLM provider and MCP servers directly (secret-free with AAuth); identity is the boundary.",
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
        <em>into</em> an agent over mTLS; the agent reaches <em>out</em> to its LLM provider and MCP
        servers directly.
      </P>
      <CodeBlock
        lang="topology"
        code={`  kubectl / operator ─┐
  A2A client ─────────┤  APIServer · A2A gateway ──mTLS client cert──▶ agent :8443 /mcp
                      │  coordination · scaler   ◀──source-IP attest── agent (claim work fabric)
  control plane ──────┘
                          agent ──AAuth-signed dial (or a mounted token)──▶ LLM provider · MCP servers
                                     (direct — no broker in path · public-HTTPS egress)
                          agent pod: serves mTLS /mcp · dials providers + MCP directly
                                     secret-free with AAuth · restricted-PSS · no hostPath`}
      />
      <Ul>
        <li>
          <strong className="text-foreground">Into the agent (Management).</strong> The APIServer
          and A2A gateway dial <C>https://&lt;podIP&gt;:8443/mcp</C> presenting the control-plane
          client cert. A cert verified against the pinned client CA is <C>Management</C>; an
          unauthenticated request is refused, never downgraded.
        </li>
        <li>
          <strong className="text-foreground">Out of the agent (direct dial).</strong> The agent
          dials its <C>INTELLIGENCE</C> endpoint and each <C>mcpServers</C> entry directly. With
          AAuth it signs each request with its own workload identity (RFC 0024), so no provider or
          tool secret rests on the pod; the fallback is a token mounted from the pool&apos;s or
          server&apos;s Secret. No broker sits in the path.
        </li>
      </Ul>

      <H2>Provisioning &amp; PKI</H2>
      <P>
        The operator renders each agent pod to serve mTLS HTTPS and mints its identity through
        cert-manager: a cluster CA <C>ClusterIssuer</C>, a per-workload serving{" "}
        <C>Certificate</C> (<C>&lt;name&gt;-serving-tls</C>), and a per-namespace{" "}
        <C>agentctl-ca</C> ConfigMap so the agent trusts the control-plane certs it dials (<C>--tls-ca</C>).
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
        The operator resolves the Agent&apos;s bound <C>model.pool</C> (a <C>ModelPool</C>) and
        renders <C>INTELLIGENCE=&lt;the pool&apos;s provider endpoint&gt;</C> into the pod; the agent
        dials that provider itself. With AAuth the dial is secret-free — the agent signs each request
        with its workload identity (RFC 0024) — and the fallback is an <C>INTELLIGENCE_TOKEN</C>{" "}
        mounted from the pool&apos;s <C>credentialSecretRef</C>. Tools work the same way:{" "}
        <C>spec.mcpServers</C> is an inline list of <C>{"{ name, endpoint, auth, tags }"}</C> the
        agent dials directly, authenticating with AAuth, a mounted <C>staticToken</C>, or{" "}
        <C>none</C>. No broker or facade sits in the path, and nothing meters a dial. The budgets
        that survive are harness-tracked:{" "}
        <C>spec.limits.lifetimeTokens</C> (cumulative) and <C>maxTokens</C> (per run), passed to the
        agent rather than enforced in path.
      </P>
      <P>
        Because agents dial providers and MCP servers directly, the operator renders an{" "}
        <C>agent-internet-egress</C> NetworkPolicy that grants each agent pod public-HTTPS egress
        (private, link-local, and CGNAT ranges carved out); lateral movement stays default-denied.
        The claim work fabric is unchanged — workers reach the <C>coordination</C> server and{" "}
        <C>scaler</C>, attested by source IP.
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
        There is no per-node agent and nothing privileged on the host. Management and A2A are mTLS
        calls between ordinary pods; inference and tools are the agent&apos;s own outbound HTTPS
        dials to its provider and MCP servers — secret-free with AAuth, no broker in the path.
      </Note>
    </DocShell>
  );
}
