"use client";
import Link from "next/link";
import { buttonClass } from "@/components/ui/Button";
import { Card, Row, Stat, StatGrid } from "@/components/ui/Card";
import { Empty } from "@/components/ui/Feedback";
import { Icon } from "@/components/ui/icons";
import { SegmentBar, StatusBadge } from "@/components/ui/Status";
import { cn } from "@/lib/cn";
import { age } from "@/lib/format";
import { useSession } from "@/lib/session";

export function OverviewPage() {
  const { me, clusters, machines, jobs, users, can } = useSession();
  const running = clusters.filter((c) => c.status === "running");
  const nodes = running.reduce((a, c) => a + c.nodes, 0);
  const nodesReady = running.reduce((a, c) => a + c.nodesReady, 0);
  const pods = running.reduce((a, c) => a + c.pods, 0);
  const podsRunning = running.reduce((a, c) => a + c.podsRunning, 0);
  const ready = machines.filter((m) => m.status === "ready").length;
  const steps = [
    { done: machines.length > 0, show: can("machines.manage"), title: "Add a machine", text: "This host or any Linux server reachable over SSH. The offline installer copies every binary and image.", href: "/machines", cta: "Add machine" },
    { done: clusters.length > 0, show: can("clusters.manage"), title: "Create a cluster", text: "Pick a master and workers. Certificates, services, kube-proxy and CoreDNS are set up for you.", href: "/clusters/new", cta: "New cluster" },
    { done: users.length > 0, show: can("users.manage"), title: "Invite your team", text: "Create users, give them console permissions and per-namespace Kubernetes roles.", href: "/users", cta: "Add user" },
  ].filter((s) => s.show);
  const todo = steps.filter((s) => !s.done);

  return (
    <>
      <StatGrid>
        <Stat value={`${running.length}/${clusters.length}`} label="Clusters running" />
        <Stat value={`${ready}/${machines.length}`} label="Machines ready" />
        <Stat value={`${nodesReady}/${nodes}`} label="Nodes ready" tone={nodes > 0 && nodesReady < nodes ? "warn" : undefined} />
        <Stat value={podsRunning} label={`Pods running of ${pods}`} tone="success" />
      </StatGrid>

      {todo.length > 0 && (
        <Card title={<><Icon name="zap" className="text-accent" />Getting started</>}>
          <ol className="grid gap-3 md:grid-cols-3">
            {steps.map((s, i) => (
              <li key={s.title} className={cn("rounded-md border border-line p-4", s.done ? "bg-subtle opacity-70" : "bg-canvas")}>
                <div className="mb-1 flex items-center gap-2">
                  <span className={cn("grid size-6 place-items-center rounded-full text-xs font-bold", s.done ? "bg-success text-white" : "bg-inset")}>{s.done ? <Icon name="check" width={12} height={12} /> : i + 1}</span>
                  <b>{s.title}</b>
                </div>
                <p className="mb-3 text-sm text-muted">{s.text}</p>
                {!s.done && <Link href={s.href} className={cn(buttonClass({ variant: "primary" }), "hover:no-underline")}>{s.cta}</Link>}
              </li>
            ))}
          </ol>
        </Card>
      )}

      <Card title="About">
        Signed in as <b>{me.username}</b>{me.superadmin ? <> (super admin)</> : me.permissions.length ? <> with {me.permissions.join(", ")}</> : null}.
        <p className="text-muted">Everything on this page updates by itself. {me.superadmin ? "You have full access to every cluster." : <>Inside clusters you act as <code>rustkubeadmin:{me.username}</code>, so Kubernetes RBAC decides what you can do.</>}</p>
      </Card>

      <Card title="Clusters" flush actions={can("clusters.manage") ? <Link href="/clusters/new" className={cn(buttonClass({ variant: "primary" }), "hover:no-underline")}>New cluster</Link> : undefined}>
        {clusters.length === 0 && <Empty>No clusters yet.{can("clusters.manage") && <> <Link href="/clusters/new">Create one</Link>.</>}</Empty>}
        {clusters.map((c) => (
          <Row key={c.name}>
            <Icon name="cluster" className="shrink-0 text-muted" />
            <Link className="w-40 truncate font-semibold" href={`/cluster?name=${encodeURIComponent(c.name)}`}>{c.name}</Link>
            <StatusBadge status={c.status === "running" && !c.reachable ? "Unknown" : c.status} label={c.status === "running" && !c.reachable ? "unreachable" : c.status} />
            <span className="hidden w-28 text-xs text-muted sm:block">{c.nodesReady}/{c.nodes} nodes</span>
            <div className="hidden min-w-0 flex-1 md:block">
              <SegmentBar total={c.pods} parts={[
                { value: c.podsRunning, className: "bg-success", label: "running" },
                { value: c.podsPending, className: "bg-warn", label: "pending" },
                { value: c.podsFailed, className: "bg-danger", label: "failing" },
              ]} />
            </div>
            <span className="ml-auto text-xs tabular-nums text-muted">{c.podsRunning}/{c.pods} pods</span>
          </Row>
        ))}
      </Card>

      <Card title="Recent activity" flush actions={<Link href="/activity" className="text-sm">All activity</Link>}>
        {jobs.length === 0 && <Empty>Nothing yet.</Empty>}
        {jobs.slice(0, 6).map((j) => (
          <Row key={j.id}>
            <Icon name={j.kind === "install" ? "download" : j.kind === "bundle" ? "file" : "cluster"} className="shrink-0 text-muted" />
            <Link href={`/activity?job=${j.id}`} className="min-w-0 flex-1 truncate">{j.title}</Link>
            <span className="hidden text-xs text-muted sm:inline">{j.user} · {age(j.started)} ago</span>
            <StatusBadge status={j.state === "running" ? "running_job" : j.state} label={j.state} />
          </Row>
        ))}
      </Card>
    </>
  );
}
