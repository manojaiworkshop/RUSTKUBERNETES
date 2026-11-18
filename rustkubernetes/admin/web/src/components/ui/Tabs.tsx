import type { ReactNode } from "react";
import { cn } from "@/lib/cn";

/** Underlined tabs; scrolls sideways when they do not fit. */
export function Tabs<T extends string>({ tabs, value, onChange, className }: { tabs: { id: T; label: ReactNode }[]; value: T; onChange: (id: T) => void; className?: string }) {
  return (
    <div role="tablist" className={cn("mb-4 flex gap-1 overflow-x-auto border-b border-line [scrollbar-width:none]", className)}>
      {tabs.map((t) => (
        <button key={t.id} role="tab" aria-selected={t.id === value} onClick={() => onChange(t.id)}
          className={cn("-mb-px cursor-pointer whitespace-nowrap border-b-2 px-3 py-2 text-sm", t.id === value ? "border-accent font-semibold text-fg" : "border-transparent text-muted hover:text-fg")}>
          {t.label}
        </button>
      ))}
    </div>
  );
}
