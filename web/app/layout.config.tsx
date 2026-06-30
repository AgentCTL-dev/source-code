import type { BaseLayoutProps } from "fumadocs-ui/layouts/shared";

// Shared between the docs layout (and any future home/notebook layouts). The
// nav title links back to the marketing landing page.
export const baseOptions: BaseLayoutProps = {
  nav: {
    title: (
      <span className="font-semibold tracking-tight">
        agentctl
        <span className="text-fd-muted-foreground"> / docs</span>
      </span>
    ),
    url: "/",
  },
  githubUrl: "https://github.com/agentctl-dev/source-code",
  links: [
    {
      text: "Docs",
      url: "/docs",
      active: "nested-url",
    },
  ],
};
