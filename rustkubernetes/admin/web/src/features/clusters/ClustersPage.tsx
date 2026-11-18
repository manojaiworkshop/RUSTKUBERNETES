"use client";
import Link from "next/link";
import { useState } from "react";
import { Button, buttonClass } from "@/components/ui/Button";
import { PageHeader } from "@/components/ui/Card";
import { Badge, Empty, useToast } from "@/components/ui/Feedback";
import { Field, Input, Textarea } from "@/components/ui/Form";
import { Icon } from "@/components/ui/icons";
import { Modal } from "@/components/ui/Modal";
import { SegmentBar, StatusBadge } from "@/components/ui/Status";
import { api } from "@/lib/api";
import { cn } from "@/lib/cn";
import { age } from "@/lib/format";
import { useSession } from "@/lib/session";

export function ClustersPage() {
  const { clusters, machines, can } = useSession();
  const toast = useToast();
  const [importing, setImporting] = useState(false);
  const [form, setForm] = useState({ name: "", kubeconfig: "" });
  const nameOf = (id: string) => machines.find((m) => m.id === id)?.name ?? id;

  return (
    <>
      <PageHeader title="Clusters" icon={<Icon name="cluster" width={20} height={20} className="text-muted" />}>
        {can("clusters.manage") && <Button icon="upload" onClick={() => setImporting(true)}>Import cluster</Button>}
        {can("clusters.manage") && <Link href="/clusters/new" className={cn(buttonClass({ variant: "primary" }), "hover:no-underline")}>New cluster</Link>}
      </PageHeader>
      {clusters.length === 0 && (
        <div className="rounded-md border border-dashed border-line"><Empty>No clusters yet. Create one from your machines, or import an existing cluster with its kubeconfig.</Empty></div>
      )}
      <div className="grid gap-4 xl:grid-cols-2">
        {clusters.map((c) => (
          <Link key={c.name} href={`/cluster?name=${encodeURIComponent(c.name)}`} className="group block rounded-md border border-line bg-canvas p-4 text-fg hover:border-link hover:no-underline">
            <div className="mb-3 flex items-center gap-2">
              <span className="grid size-9 place-items-center rounded-md bg-gradient-to-br from-[#f78166]/20 to-[#a371f7]/25 text-accent"><Icon name="cluster" /></span>
              <div className="min-w-0 flex-1">
                <b className="block truncate text-base group-hover:text-link">{c.name}</b>
                <span className="block truncate font-mono text-xs text-muted">{c.endpoint}</span>
              </div>
              <StatusBadge status={c.status === "running" && !c.reachable ? "Unknown" : c.status} label={c.status === "running" && !c.reachable ? "unreachable" : c.status} />
            </div>
            <div className="mb-3 grid grid-cols-4 gap-2 text-center">
              {[["Nodes", `${c.nodesReady}/${c.nodes}`], ["Pods", `${c.podsRunning}/${c.pods}`], ["Deployments", c.deployments], ["Services", c.services]].map(([l, v]) => (
                <div key={l} className="rounded-md bg-subtle py-2"><b className="block tabular-nums">{v}</b><span className="text-xs text-muted">{l}</span></div>
              ))}
            </div>
            <SegmentBar total={c.pods} parts={[
              { value: c.podsRunning, className: "bg-success", label: "running" },
              { value: c.podsPending, className: "bg-warn", label: "pending" },
              { value: c.podsFailed, className: "bg-danger", label: "failing" },
            ]} />
            <div className="mt-3 flex flex-wrap items-center gap-2 text-xs text-muted">
              <Badge>{c.kind}</Badge>
              {c.kind === "managed" && <span>master <b className="text-fg">{nameOf(c.master)}</b>{c.workers.length > 0 && <> · {c.workers.length} worker{c.workers.length > 1 && "s"}</>}</span>}
              {c.dns && <Badge tone="info">CoreDNS</Badge>}
              <span className="ml-auto">by {c.createdBy} · {age(c.created)} ago</span>
            </div>
            {c.status === "failed" && c.message && <p className="mt-2 truncate text-xs text-danger">{c.message}</p>}
          </Link>
        ))}
      </div>
      {importing && (
        <Modal title="Import a cluster" confirmLabel="Import" wide onClose={() => setImporting(false)} onConfirm={async () => {
          await api("/api/clusters/import", form);
          toast(`Cluster ${form.name} imported`);
          setForm({ name: "", kubeconfig: "" });
        }}>
          <p className="text-muted">Connect any running rustkubernetes (or Kubernetes) cluster. The kubeconfig must belong to a cluster admin: the console impersonates its users and manages their RBAC bindings.</p>
          <Field label="Name" hint="lowercase letters, digits and -"><Input value={form.name} onChange={(e) => setForm({ ...form, name: e.target.value })} autoFocus required /></Field>
          <Field label="Kubeconfig" hint="paste the YAML (e.g. _output/local/kubeconfig)"><Textarea rows={12} className="font-mono text-xs" value={form.kubeconfig} onChange={(e) => setForm({ ...form, kubeconfig: e.target.value })} required /></Field>
        </Modal>
      )}
    </>
  );
}
