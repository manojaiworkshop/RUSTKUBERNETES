import type { ReactNode } from "react";
import { cn } from "@/lib/cn";

/** A bordered panel. `title`/`actions` make the header bar; `flush` drops the body padding (tables, lists, grids). */
export function Card({ title, actions, children, flush, className }: { title?: ReactNode; actions?: ReactNode; children?: ReactNode; flush?: boolean; className?: string }) {
  return (
    <section className={cn("mb-4 overflow-hidden rounded-md border border-line bg-canvas", className)}>
      {(title || actions) && (
        <header className="flex min-h-10 flex-wrap items-center gap-2 border-b border-line bg-subtle px-4 py-1.5">
          <h2 className="mr-auto flex min-w-0 items-center gap-2 text-sm font-semibold">{title}</h2>
          {actions}
        </header>
      )}
      <div className={flush ? "" : "p-4"}>{children}</div>
    </section>
  );
}

/** One line of a list inside a flush Card. */
export function Row({ children, onClick, className }: { children: ReactNode; onClick?: () => void; className?: string }) {
  return (
    <div onClick={onClick} className={cn("flex items-center gap-2.5 border-t border-line px-4 py-2 first:border-t-0 hover:bg-subtle", onClick && "cursor-pointer", className)}>
      {children}
    </div>
  );
}

/** A big number with a caption. */
export function Stat({ value, label, tone, children }: { value: ReactNode; label: string; tone?: "success" | "warn"; children?: ReactNode }) {
  return (
    <div className="rounded-md border border-line bg-subtle p-4">
      <b className={cn("block truncate text-2xl font-semibold tabular-nums sm:text-[28px]", tone === "success" && "text-success", tone === "warn" && "text-warn")}>{value}</b>
      <span className="text-muted">{label}</span>
      {children}
    </div>
  );
}

export function StatGrid({ children }: { children: ReactNode }) {
  return <div className="mb-4 grid grid-cols-2 gap-3 sm:gap-4 lg:grid-cols-4">{children}</div>;
}

export function PageHeader({ title, icon, children }: { title: ReactNode; icon?: ReactNode; children?: ReactNode }) {
  return (
    <div className="mb-4 flex flex-wrap items-center gap-2">
      <h1 className="mr-auto flex min-w-0 items-center gap-2 text-xl font-semibold [overflow-wrap:anywhere]">{icon}{title}</h1>
      {children}
    </div>
  );
}
