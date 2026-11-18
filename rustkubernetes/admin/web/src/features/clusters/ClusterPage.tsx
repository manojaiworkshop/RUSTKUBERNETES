"use client";
import { useRouter, useSearchParams } from "next/navigation";
import { useState } from "react";
import { Button, ButtonLink, IconButton } from "@/components/ui/Button";
import { PageHeader, Stat, StatGrid } from "@/components/ui/Card";
import { Alert, Badge, useToast } from "@/components/ui/Feedback";
import { Field, Input } from "@/components/ui/Form";
import { Icon } from "@/components/ui/icons";
import { JobLog } from "@/components/ui/JobLog";
import { Modal } from "@/components/ui/Modal";
import { StatusBadge } from "@/components/ui/Status";
import { Tabs } from "@/components/ui/Tabs";
import { api } from "@/lib/api";
import { copyText } from "@/lib/clipboard";
import { useSession } from "@/lib/session";
import type { ClusterSnapshot } from "@/lib/types";
import { useEvents } from "@/lib/useEvents";
import { AccessTab } from "./AccessTab";
import { ApplyTab } from "./ApplyTab";
import { ControlPlaneTab } from "./ControlPlaneTab";
import { EventsTab, NamespacesTab, NodesTab, OverviewTab, ServicesTab } from "./InfoTabs";
import { PodsTab } from "./PodsTab";
import { WorkloadsTab } from "./WorkloadsTab";

type TabId = "overview" | "pods" | "workloads" | "services" | "nodes" | "namespaces" | "events" | "apply" | "access" | "control";

const EMPTY: ClusterSnapshot = { reachable: false, nodes: [], pods: [], deployments: [], services: [], namespaces: [], events: [] };

export function ClusterPage() {
  const sp = useSearchParams();
  const router = useRouter();
  const name = sp.get("name") ?? "";
  const tab = (sp.get("tab") as TabId) || "overview";
  const { clusters, jobs, can, me } = useSession();
  const toast = useToast();
  const [snap, setSnap] = useState<ClusterSnapshot | null>(null);
  const [deleting, setDeleting] = useState(false);
  const [confirmName, setConfirmName] = useState("");
  useEvents([`cluster:${name}`], { [`cluster:${name}`]: (s: ClusterSnapshot) => setSnap(s) }, !!name);
  const c = clusters.find((x) => x.name === name);
  const s = snap ?? EMPTY;
  const job = jobs.find((j) => j.target === name);
  const setTab = (t: TabId) => router.replace(`/cluster?name=${encodeURIComponent(name)}&tab=${t}`);

  if (!c) return <Alert tone="info">Cluster <b>{name}</b> was not found (it may have been deleted, or you have no access to it).</Alert>;

  const act = async (action: string) => {
    await api("/api/clusters/action", { name, action });
    toast(`${action[0].toUpperCase()}${action.slice(1)} started`);
  };
  const busy = job?.state === "running";
  const managed = c.kind === "managed";
  const tabs: { id: TabId; label: React.ReactNode }[] = [
    { id: "overview", label: "Overview" },
    { id: "pods", label: <>Pods <span className="rounded-full bg-inset px-1.5 text-xs">{s.pods.length}</span></> },
    { id: "workloads", label: "Workloads" },
    { id: "services", label: "Services" },
    ...(!s.restricted ? [{ id: "nodes" as TabId, label: <>Nodes <span className="rounded-full bg-inset px-1.5 text-xs">{s.nodes.length}</span></> }] : []),
    { id: "namespaces", label: "Namespaces" },
    { id: "events", label: "Events" },
    { id: "apply", label: "Apply YAML" },
    { id: "access", label: "Access & RBAC" },
    ...(managed && can("clusters.manage") ? [{ id: "control" as TabId, label: "Control plane" }] : []),
  ];

  return (
    <>
      <PageHeader title={c.name} icon={<span className="grid size-8 place-items-center rounded-md bg-gradient-to-br from-[#f78166]/20 to-[#a371f7]/25 text-accent"><Icon name="cluster" /></span>}>
        <StatusBadge status={c.status === "running" && !c.reachable ? "Unknown" : c.status} label={c.status === "running" && !c.reachable ? "unreachable" : c.status} />
        <Badge>{c.kind}</Badge>
        <ButtonLink icon="download" href={`/api/clusters/kubeconfig?name=${encodeURIComponent(name)}`}>Kubeconfig</ButtonLink>
        {can("clusters.manage") && managed && c.status === "running" && <Button icon="stop" disabled={busy} onClick={() => act("stop")}>Stop</Button>}
        {can("clusters.manage") && managed && c.status === "stopped" && <Button icon="play" variant="primary" disabled={busy} onClick={() => act("start")}>Start</Button>}
        {can("clusters.manage") && c.status === "failed" && <Button icon="sync" variant="primary" disabled={busy} onClick={() => act("retry")}>Retry</Button>}
        {can("clusters.manage") && <IconButton icon="trash" label="Delete cluster" variant="danger" disabled={busy} onClick={() => { setConfirmName(""); setDeleting(true); }} />}
      </PageHeader>
      <div className="mb-4 flex flex-wrap items-center gap-2 text-sm text-muted">
        <code className="rounded bg-inset px-2 py-0.5 text-fg">{c.endpoint}</code>
        <IconButton icon="copy" label="Copy API endpoint" variant="ghost" onClick={async () => { await copyText(c.endpoint); toast("Copied"); }} />
        <span>created by {c.createdBy}</span>
        {!me.superadmin && <span>· you act as <code>rustkubeadmin:{me.username}</code></span>}
      </div>
      {c.status === "failed" && c.message && <Alert>{c.message}</Alert>}
      {job && (job.state === "running" || (job.state === "failed" && c.status !== "running")) && <div className="mb-4"><JobLog job={job} /></div>}

      {c.status === "running" && (
        <>
          <StatGrid>
            {!s.restricted && <Stat value={`${s.nodes.filter((n) => n.ready).length}/${s.nodes.length}`} label="Nodes ready" />}
            <Stat value={s.pods.filter((p) => p.status === "Running").length} label={`Pods running of ${s.pods.length}`} tone="success" />
            <Stat value={s.deployments.length} label="Deployments" />
            <Stat value={s.services.length} label="Services" />
            {s.restricted && <Stat value={s.namespaces.length} label="Your namespaces" />}
          </StatGrid>
          {!s.reachable && snap && <Alert>The API server does not answer right now. The data below is the last known state.</Alert>}
          {s.restricted && <Alert tone="info">You see the namespaces you were given access to: {s.namespaces.map((n) => n.name).join(", ") || "none yet"}.</Alert>}
          <Tabs tabs={tabs} value={tab} onChange={setTab} />
          {tab === "overview" && <OverviewTab cluster={name} s={s} onTab={(t) => setTab(t as TabId)} />}
          {tab === "pods" && <PodsTab cluster={name} s={s} />}
          {tab === "workloads" && <WorkloadsTab cluster={name} s={s} />}
          {tab === "services" && <ServicesTab s={s} />}
          {tab === "nodes" && <NodesTab cluster={name} s={s} />}
          {tab === "namespaces" && <NamespacesTab cluster={name} s={s} />}
          {tab === "events" && <EventsTab s={s} />}
          {tab === "apply" && <ApplyTab cluster={name} s={s} />}
          {tab === "access" && <AccessTab cluster={name} s={s} />}
          {tab === "control" && <ControlPlaneTab cluster={name} />}
        </>
      )}
      {c.status === "stopped" && <Alert tone="info">The cluster is stopped. Start it to see nodes and workloads (pods keep their state).</Alert>}
      {c.status === "stopped" && managed && can("clusters.manage") && <ControlPlaneTab cluster={name} />}

      {deleting && (
        <Modal title={`Delete cluster ${name}`} danger confirmLabel="Delete cluster" onClose={() => setDeleting(false)} onConfirm={async () => {
          if (confirmName !== name) throw new Error("Type the cluster name to confirm.");
          await act("delete");
          router.push("/clusters");
        }}>
          <p>{managed ? "This stops every service of the cluster on its machines, removes its data, certificates and pods, and frees the machines." : "This removes the imported cluster from the console (the cluster itself is not touched)."}</p>
          <Field label={<>Type <code>{name}</code> to confirm</>}><Input value={confirmName} onChange={(e) => setConfirmName(e.target.value)} autoFocus /></Field>
        </Modal>
      )}
    </>
  );
}
