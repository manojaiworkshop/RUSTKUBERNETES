"use client";
import Link from "next/link";
import { usePathname, useRouter } from "next/navigation";
import { Suspense, useEffect, useState, type ReactNode } from "react";
import { Button, IconButton } from "@/components/ui/Button";
import { Badge } from "@/components/ui/Feedback";
import { api } from "@/lib/api";
import { cn } from "@/lib/cn";
import { useSession } from "@/lib/session";
import { useTheme } from "@/lib/theme";
import { Sidebar } from "./Sidebar";

/** The frame around every page of the console: header, section tabs, the cluster/machine list and the page itself. */
export function AppShell({ children }: { children: ReactNode }) {
  const { me, clusters, machines, jobs, live, can } = useSession();
  const pathname = usePathname();
  const router = useRouter();
  const [theme, toggleTheme] = useTheme();
  const [drawer, setDrawer] = useState(false);
  const running = jobs.filter((j) => j.state === "running").length;

  const NAV = [
    { href: "/overview", label: "Overview", show: true },
    { href: "/clusters", label: "Clusters", count: clusters.length, show: true },
    { href: "/machines", label: "Machines", count: machines.length, show: true },
    { href: "/users", label: "Users & access", show: can("users.manage") },
    { href: "/activity", label: "Activity", badge: running, show: true },
    { href: "/settings", label: "Settings", show: true },
  ];

  useEffect(() => setDrawer(false), [pathname]);
  const signOut = async () => {
    await api("/api/logout", {}).catch(() => {});
    router.replace("/login");
  };

  return (
    <div className="flex min-h-dvh flex-col">
      <header className="border-b border-line bg-subtle px-3 pt-3 sm:px-6 sm:pt-4">
        <div className="mb-3 flex items-center gap-2 sm:gap-3">
          <IconButton icon="menu" label="Clusters and machines" className="lg:hidden" onClick={() => setDrawer(true)} />
          <span className="grid size-8 shrink-0 place-items-center rounded-full bg-gradient-to-br from-[#f78166] to-[#a371f7] font-bold text-white">K</span>
          <div className="flex min-w-0 items-baseline gap-1 text-base sm:text-xl">
            <span className="truncate text-muted">{me.username}</span>
            <span className="text-muted">/</span>
            <b className="truncate font-semibold">rustkubeadmin</b>
          </div>
          <span className="hidden sm:inline">{me.superadmin ? <Badge tone="warn">super admin</Badge> : <Badge>{me.permissions.length ? `${me.permissions.length} permissions` : "user"}</Badge>}</span>
          <span className="ml-auto" />
          <span title={live ? "Live updates are on" : "Connecting to live updates…"} className="hidden items-center gap-1.5 text-xs text-muted sm:flex">
            <span className={cn("size-2 rounded-full", live ? "bg-success" : "bg-warn")} />{live ? "Live" : "Offline"}
          </span>
          <IconButton icon={theme === "dark" ? "sun" : "moon"} label="Switch theme" onClick={toggleTheme} />
          <Button onClick={signOut}>Sign out</Button>
        </div>
        <nav aria-label="Sections" className="-mb-px flex gap-1 overflow-x-auto [scrollbar-width:none]">
          {NAV.filter((n) => n.show).map((n) => {
            const on = pathname.startsWith(n.href) || (n.href === "/clusters" && pathname.startsWith("/cluster"));
            return (
              <Link key={n.href} href={n.href} aria-current={on ? "page" : undefined}
                className={cn("flex items-center gap-2 whitespace-nowrap rounded-t-md border-b-2 px-3 py-2 text-fg hover:bg-inset hover:no-underline", on ? "border-accent font-semibold" : "border-transparent")}>
                {n.label}
                {n.count !== undefined && <span className="rounded-full bg-inset px-1.5 text-xs">{n.count}</span>}
                {!!n.badge && <span className="rounded-full bg-link px-1.5 text-xs text-white">{n.badge}</span>}
              </Link>
            );
          })}
        </nav>
      </header>

      <div className="mx-auto grid w-full max-w-[1500px] flex-1 grid-cols-1 gap-6 px-3 py-4 sm:px-6 sm:py-6 lg:grid-cols-[290px_minmax(0,1fr)]">
        <aside className="hidden lg:block">
          <div className="sticky top-4"><Suspense><Sidebar /></Suspense></div>
        </aside>
        <main className="min-w-0">{children}</main>
      </div>

      {drawer && (
        <div className="fixed inset-0 z-30 bg-overlay lg:hidden" onClick={() => setDrawer(false)}>
          <div className="h-full w-[min(22rem,88vw)] overflow-y-auto bg-canvas p-3 shadow-2xl" onClick={(e) => e.stopPropagation()}>
            <Suspense><Sidebar onNavigate={() => setDrawer(false)} /></Suspense>
          </div>
        </div>
      )}
    </div>
  );
}
