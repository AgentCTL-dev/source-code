import "./globals.css";
import type { ReactNode } from "react";
import type { Metadata } from "next";
import Script from "next/script";
import { ThemeProvider } from "@/components/site/theme-provider";

export const metadata: Metadata = {
  metadataBase: new URL("https://agentctl.dev"),
  title: {
    default:
      "agentctl — the Kubernetes control plane for fleets of conformant agents",
    template: "%s · agentctl",
  },
  description:
    "agentctl is a Kubernetes control plane for provisioning, scaling, securing, " +
    "and observing fleets of contract-conformant agents. Agents serve mTLS HTTPS " +
    "and dial their LLM provider and MCP servers directly — identity is the boundary, " +
    "and with AAuth no model or tool secret lands on the pod.",
};

export default function RootLayout({ children }: { children: ReactNode }) {
  return (
    <html lang="en" suppressHydrationWarning>
      <body className="flex min-h-screen flex-col font-sans antialiased">
        <Script
          defer
          strategy="afterInteractive"
          data-domain="agentctl.dev"
          src="https://analytics.tsok.org/js/script.js"
        />
        <ThemeProvider>{children}</ThemeProvider>
      </body>
    </html>
  );
}
