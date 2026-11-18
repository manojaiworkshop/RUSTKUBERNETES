"use client";
import Link from "next/link";
import { useRouter } from "next/navigation";
import { useMemo, useState, type FormEvent } from "react";
import { Button } from "@/components/ui/Button";
import { Card, PageHeader } from "@/components/ui/Card";
import { Alert, Badge } from "@/components/ui/Feedback";
import { Checkbox, Field, Input, Select } from "@/components/ui/Form";
import { Icon } from "@/components/ui/icons";
import { JobLog } from "@/components/ui/JobLog";
import { StatusBadge } from "@/components/ui/Status";
import { api } from "@/lib/api";
import { cn } from "@/lib/cn";
import { memory } from "@/lib/format";
import { useSession } from "@/lib/session";
import type { Machine, Ports } from "@/lib/types";

const DEFAULT_PORTS: Ports = { api: 6443, kubelet: 10250, controller_manager: 10257, scheduler: 10259, proxy_healthz: 10256, proxy_metrics: 10249 };

function MachineCard({ m, role }: { m: Machine; role: "master" | "worker" }) {
  return (
    <div className={cn("rounded-md border p-3", role === "master" ? "border-accent/60 bg-accent/5" : "border-line bg-canvas")}>
      <div className="flex items-center gap-2">
        <Icon name="server" className="text-muted" />
        <b className="truncate">{m.name}</b>
        <Badge tone={role === "master" ? "warn" : "info"} className="ml-auto">{role}</Badge>
      </div>
      <div className="mt-1 font-mono text-xs text-muted">{m.address}</div>
      <div className="mt-1 text-xs text-muted">{m.facts.cpus ?? "?"} CPUs · {memory(m.facts.memory_kb ? `${m.facts.memory_kb}Ki` : undefined)}</div>
      <div className="mt-2 flex flex-wrap gap-1">
        {(role === "master" ? ["apiserver", "controller-manager", "scheduler", "kubelet", "kube-proxy"] : ["kubelet", "kube-proxy"]).map((s) => (
          <span key={s} className="rounded bg-inset px-1.5 py-0.5 font-mono text-[11px]">{s}</span>
        ))}
      </div>
    </div>
  );
}

export function NewClusterPage() {
  const { machines, jobs, can } = useSession();
  const router = useRouter();
  const free = machines.filter((m) => !m.cluster);
  const ready = free.filter((m) => m.status === "ready");
  const [name, setName] = useState("");
  const [master, setMaster] = useState("");
  const [workers, setWorkers] = useState<string[]>([]);
  const [dns, setDns] = useState(true);
  const [proxy, setProxy] = useState(true);
  const [ports, setPorts] = useState<Ports>(DEFAULT_PORTS);
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  const [jobId, setJobId] = useState<string | null>(null);
  const job = jobs.find((j) => j.id === jobId);
  const masterId = master || ready[0]?.id || "";
  const masterM = machines.find((m) => m.id === masterId);
  const workerMs = useMemo(() => machines.filter((m) => workers.includes(m.id) && m.id !== masterId), [machines, workers, masterId]);

  if (!can("clusters.manage")) return <Alert>You need the clusters.manage permission to create clusters.</Alert>;

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    setBusy(true);
    setError("");
    try {
      const r = await api<{ job: string }>("/api/clusters", { name, master: masterId, workers: workerMs.map((m) => m.id), dns, kubeProxy: proxy, ports });
      setJobId(r.job);
    } catch (x) {
      setError((x as Error).message);
    } finally {
      setBusy(false);
    }
  };

  if (job) {
    return (
      <>
        <PageHeader title={<>Creating cluster <span className="text-accent">{name}</span></>} icon={<Icon name="cluster" width={20} height={20} className="text-muted" />}>
          <StatusBadge status={job.state === "running" ? "provisioning" : job.state} label={job.state === "running" ? "provisioning" : job.state} />
        </PageHeader>
        <JobLog job={job} />
        <div className="mt-4 flex gap-2">
          {job.state === "succeeded" && <Button variant="primary" onClick={() => router.push(`/cluster?name=${encodeURIComponent(name)}`)}>Open cluster</Button>}
          {job.state === "failed" && <Button onClick={() => router.push(`/cluster?name=${encodeURIComponent(name)}`)}>Open cluster (retry from there)</Button>}
        </div>
      </>
    );
  }

  return (
    <>
      <PageHeader title="New cluster" icon={<Icon name="cluster" width={20} height={20} className="text-muted" />} />
      {ready.length === 0 && (
        <Alert tone="info">No free machine is ready. <Link href="/machines">Add a machine</Link> first — the offline installer prepares it in a minute.</Alert>
      )}
      {error && <Alert>{error}</Alert>}
      <form onSubmit={submit} className="grid gap-4 xl:grid-cols-[minmax(0,1fr)_380px]">
        <div>
          <Card title="1 · Name">
            <Field label="Cluster name" hint="lowercase letters, digits and -"><Input value={name} onChange={(e) => setName(e.target.value.toLowerCase())} placeholder="prod" required pattern="[a-z0-9]([a-z0-9-]*[a-z0-9])?" maxLength={30} /></Field>
          </Card>
          <Card title="2 · Master (control plane)">
            <Field label="Master machine" hint="runs the API server, controller manager, scheduler and also schedules pods">
              <Select value={masterId} onChange={(e) => setMaster(e.target.value)} required>
                {ready.length === 0 && <option value="">no ready machine</option>}
                {ready.map((m) => <option key={m.id} value={m.id}>{m.name} — {m.address}</option>)}
              </Select>
            </Field>
          </Card>
          <Card title="3 · Workers" actions={<span className="text-xs text-muted">optional</span>}>
            {free.filter((m) => m.id !== masterId).length === 0 && <p className="text-muted">No other free machines. A single-node cluster runs pods on the master.</p>}
            <div className="grid gap-2 sm:grid-cols-2">
              {free.filter((m) => m.id !== masterId).map((m) => (
                <label key={m.id} className={cn("flex cursor-pointer items-center gap-3 rounded-md border p-3", workers.includes(m.id) ? "border-link bg-sel" : "border-line", m.status !== "ready" && "cursor-not-allowed opacity-60")}>
                  <input type="checkbox" className="accent-[var(--link)]" disabled={m.status !== "ready"} checked={workers.includes(m.id)}
                    onChange={(e) => setWorkers((w) => e.target.checked ? [...w, m.id] : w.filter((x) => x !== m.id))} />
                  <Icon name="server" className="text-muted" />
                  <span className="min-w-0 flex-1"><b className="block truncate">{m.name}</b><span className="font-mono text-xs text-muted">{m.address}</span></span>
                  <StatusBadge status={m.status} />
                </label>
              ))}
            </div>
          </Card>
          <Card title="4 · Add-ons">
            <div className="space-y-2">
              <Checkbox label={<><b>CoreDNS</b> <span className="text-muted">— cluster DNS at 10.96.0.10 (service names like <code>web.default.svc</code>)</span></>} checked={dns} onChange={(e) => setDns(e.target.checked)} />
              <Checkbox label={<><b>kube-proxy</b> <span className="text-muted">— Service ClusterIPs and NodePorts (nftables)</span></>} checked={proxy} onChange={(e) => { setProxy(e.target.checked); if (!e.target.checked) setDns(false); }} />
            </div>
            <details className="mt-4">
              <summary className="cursor-pointer text-sm text-link">Advanced: ports</summary>
              <p className="mt-2 text-xs text-muted">Change these when the machines already run something on the default ports (for example another cluster).</p>
              <div className="mt-3 grid grid-cols-2 gap-3 sm:grid-cols-3">
                {(Object.keys(DEFAULT_PORTS) as (keyof Ports)[]).map((k) => (
                  <Field key={k} label={k.replace("_", " ")}><Input type="number" min={1} max={65535} value={ports[k]} onChange={(e) => setPorts({ ...ports, [k]: Number(e.target.value) })} /></Field>
                ))}
              </div>
            </details>
          </Card>
        </div>
        <div>
          <div className="sticky top-4">
            <Card title="Topology">
              {masterM ? (
                <div className="space-y-3">
                  <MachineCard m={masterM} role="master" />
                  {workerMs.length > 0 && <div className="mx-auto h-4 w-px bg-line" />}
                  {workerMs.map((m) => <MachineCard key={m.id} m={m} role="worker" />)}
                  <div className="rounded-md bg-subtle p-3 text-xs text-muted">
                    API <code className="text-fg">https://{masterM.address}:{ports.api}</code><br />
                    {1 + workerMs.length} node{workerMs.length ? "s" : ""} · TLS, RBAC and ServiceAccount tokens on · {dns ? "CoreDNS" : "no DNS"} · {proxy ? "kube-proxy" : "no Service proxy"}
                  </div>
                </div>
              ) : <p className="text-muted">Pick a master.</p>}
            </Card>
            <Button type="submit" variant="primary" block lg disabled={busy || !masterId || !name}>{busy ? "Starting…" : "Create cluster"}</Button>
          </div>
        </div>
      </form>
    </>
  );
}
