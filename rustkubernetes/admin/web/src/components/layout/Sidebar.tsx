"use client";
import Link from "next/link";
import { usePathname, useSearchParams } from "next/navigation";
import { buttonClass } from "@/components/ui/Button";
import { Icon } from "@/components/ui/icons";
import { Dot, toneOf } from "@/components/ui/Status";
import { cn } from "@/lib/cn";
import { useSession } from "@/lib/session";

/** The left panel: clusters (with live pod counts) and machines. `onNavigate` closes the drawer on a phone. */
export function Sidebar({ onNavigate }: { onNavigate?: () => void }) {
  const { clusters, machines, can } = useSession();
  const sp = useSearchParams();
  const pathname = usePathname();
  const current = pathname.startsWith("/cluster") ? sp.get("name") : null;
  const item = "flex items-center gap-2 border-l-2 px-3 py-1.5 text-fg hover:bg-subtle hover:no-underline";

  return (
    <nav aria-label="Clusters and machines" className="overflow-hidden rounded-md border border-line bg-canvas">
      <div className="flex items-center gap-2 border-b border-line bg-subtle px-3 py-2 font-semibold">
        <Icon name="folder" className="shrink-0 text-muted" />
        <span className="mr-auto">Resources</span>
        {can("clusters.manage") && <Link href="/clusters/new" onClick={onNavigate} className={cn(buttonClass({ variant: "primary" }), "hover:no-underline")}>New cluster</Link>}
      </div>
      <div className="max-h-[70dvh] overflow-y-auto lg:max-h-[calc(100dvh-13rem)]">
        <div className="border-b border-line bg-subtle px-3 py-1.5 text-xs font-semibold uppercase tracking-wide text-muted">Clusters <span className="font-normal">{clusters.length}</span></div>
        {clusters.map((c) => (
          <Link key={c.name} href={`/cluster?name=${encodeURIComponent(c.name)}`} onClick={onNavigate}
            className={cn(item, current === c.name ? "border-accent bg-sel font-semibold" : "border-transparent")}>
            <Icon name="cluster" className="shrink-0 text-muted" />
            <span className="mr-auto truncate">{c.name}</span>
            {c.status === "running" ? <span className="text-xs tabular-nums text-muted">{c.podsRunning}/{c.pods}</span> : <span className="text-xs text-muted">{c.status}</span>}
            <Dot tone={c.status === "running" ? (c.reachable ? "success" : "danger") : toneOf(c.status)} />
          </Link>
        ))}
        {clusters.length === 0 && <p className="px-3 py-2 text-muted">No clusters yet.</p>}
        <div className="border-y border-line bg-subtle px-3 py-1.5 text-xs font-semibold uppercase tracking-wide text-muted">Machines <span className="font-normal">{machines.length}</span></div>
        {machines.map((m) => (
          <Link key={m.id} href={`/machines?id=${m.id}`} onClick={onNavigate} className={cn(item, "border-transparent")}>
            <Icon name="server" className="shrink-0 text-muted" />
            <span className="mr-auto truncate">{m.name}</span>
            {m.role && <span className="text-xs text-muted">{m.role}</span>}
            <Dot tone={toneOf(m.status)} />
          </Link>
        ))}
        {machines.length === 0 && <p className="px-3 py-2 text-muted">No machines yet.</p>}
      </div>
    </nav>
  );
}
