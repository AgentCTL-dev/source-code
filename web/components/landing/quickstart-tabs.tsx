"use client";

import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { CodePanel } from "@/components/landing/code-panel";

export type QuickstartBlock = { title: string; html: string; raw: string };
export type QuickstartTab = {
  value: string;
  label: string;
  blurb: string;
  blocks: QuickstartBlock[];
};

export function QuickstartTabs({ tabs }: { tabs: QuickstartTab[] }) {
  return (
    <Tabs defaultValue={tabs[0]?.value} className="w-full gap-5">
      <div className="overflow-x-auto">
        <TabsList className="h-auto flex-nowrap">
          {tabs.map((t) => (
            <TabsTrigger key={t.value} value={t.value} className="whitespace-nowrap">
              {t.label}
            </TabsTrigger>
          ))}
        </TabsList>
      </div>
      {tabs.map((t) => (
        <TabsContent key={t.value} value={t.value} className="space-y-4">
          <p className="text-muted-foreground text-sm">{t.blurb}</p>
          {t.blocks.map((b) => (
            <CodePanel
              key={b.title}
              title={b.title}
              html={b.html}
              raw={b.raw}
              showDots={false}
            />
          ))}
        </TabsContent>
      ))}
    </Tabs>
  );
}
