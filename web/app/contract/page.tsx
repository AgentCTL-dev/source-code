import type { Metadata } from "next";
import { DocShell, H2, P, Ul, C, Note } from "@/components/site/doc-shell";
import { CodeBlock } from "@/components/site/code-block";
import { REPO_CONTRACT, rfc } from "@/data/site";

export const metadata: Metadata = {
  title: "The Agent Control Contract",
  description:
    "The neutral, machine-readable contract agentctl consumes and any conformant agent implements. Contract 2.0: mTLS HTTPS, bare-PascalCase A2A, identity-is-the-boundary.",
};

export default function Page() {
  return (
    <DocShell
      title="The Agent Control Contract"
      lead="A neutral, language-neutral, machine-readable contract — published as JSON Schemas plus golden fixtures. agentctl consumes only this; agentd is the reference implementation, not a dependency."
      editHref={REPO_CONTRACT}
    >
      <H2>P0 — depend on the contract, never on an agent</H2>
      <P>
        The foundational principle: agentctl depends on the <strong>contract</strong>, not on any
        one agent binary. Any binary that emits a conformant capabilities manifest, honours the
        frozen exit-code table, serves the surfaces it declares, and speaks the declared wire
        protocols is manageable — unchanged. The tokens are vendor-neutral (<C>AGENT_*</C> env,{" "}
        <C>agent://</C> URIs, the <C>agent_</C> metric prefix).
      </P>

      <H2>The capabilities manifest</H2>
      <P>
        The discovery spine. An agent emits it from <C>--capabilities</C> and the live{" "}
        <C>agent://capabilities</C> resource. The <C>surfaces{"{}"}</C> block is the single place a
        consumer learns what is served — a key absent means the surface is unbuilt, so the control
        plane degrades gracefully and never branches on <C>build_features</C>.
      </P>
      <CodeBlock
        lang="surfaces{} — contract 2.0 (excerpt)"
        code={`"contract_version": "2.0",
"surfaces": {
  "management": "https://0.0.0.0:8443",   // mTLS https URL (was unix/vsock)
  "a2a": { "streaming": true,
           "methods": ["SendMessage","GetTask",...] },  // bare PascalCase
  "operator_tools": ["a2a.Drain","a2a.LameDuck",
                     "a2a.Pause","a2a.Resume","a2a.Cancel"],
  "metrics": "0.0.0.0:9090"
},
"exec_enabled": false                     // exec surface removed in 2.0`}
      />

      <H2>What changed in 2.0</H2>
      <Ul>
        <li>
          <strong className="text-foreground">Transports.</strong> stdio / unix / vsock removed —
          agents serve over mTLS HTTPS <C>POST /mcp</C> and dial the gateways keyless.
        </li>
        <li>
          <strong className="text-foreground">Identity is authority.</strong> A verified mTLS
          client cert is <C>Management</C>; reachability is no longer authorization.
        </li>
        <li>
          <strong className="text-foreground">A2A binding resolved.</strong> Bare PascalCase over
          HTTPS is normative; SSE streaming terminates on the terminal task state (no{" "}
          <C>final</C> flag). Config MCP servers and A2A peers are HTTPS endpoints.
        </li>
        <li>
          <strong className="text-foreground">exec removed.</strong> Agents work only through
          operator-provided MCP tools — no local execution surface.
        </li>
      </Ul>

      <H2>Secret-freedom is structural</H2>
      <P>
        The manifest never carries credentials — <C>intelligence</C> is structural only (transport
        scheme + endpoint count + health), never a URL or token. Credentials travel only the
        gateway path and are injected off-pod. The config file carries references, never resolved
        values.
      </P>

      <Note>
        Shape is necessary but not sufficient: a binary that parses but misbehaves is
        non-conformant. The behavioral conformance suite is the executable definition of &ldquo;a
        conformant agent.&rdquo; The full spec and schemas — and{" "}
        <a
          href={rfc("0021", "contract-2.0-network-substrate-pivot")}
          className="text-foreground underline underline-offset-4"
          target="_blank"
          rel="noreferrer"
        >
          RFC 0021
        </a>{" "}
        for the pivot rationale — live in the repo.
      </Note>
    </DocShell>
  );
}
