import "./globals.css";
import type { ReactNode } from "react";
import type { Metadata } from "next";
import { RootProvider } from "fumadocs-ui/provider";
import { searchOptions } from "@/lib/search-client";
import Script from "next/script";

export const metadata: Metadata = {
  title: {
    default:
      "agentctl — the Kubernetes control plane for fleets of conformant agents",
    template: "%s · agentctl",
  },
  description:
    "agentctl is a Kubernetes control plane for declaratively provisioning, " +
    "scaling, securing, and observing fleets of contract-conformant AI agents " +
    "(ACC). Operator, node-agent, aggregated APIServer, A2A gateway, " +
    "ModelGateway, coordination + scaling planes — every capability gated " +
    "default-off.",
};

export default function RootLayout({ children }: { children: ReactNode }) {
  return (
    <html lang="en" suppressHydrationWarning>
      <body className="flex min-h-screen flex-col font-sans antialiased">
        <Script defer strategy="afterInteractive" data-domain="agentctl.dev" src="https://analytics.tsok.org/js/script.js" />
        <RootProvider
          theme={{
            defaultTheme: "dark",
            enableSystem: true,
            attribute: "class",
          }}
          search={searchOptions}
        >
          {children}
        </RootProvider>
      </body>
    </html>
  );
}
