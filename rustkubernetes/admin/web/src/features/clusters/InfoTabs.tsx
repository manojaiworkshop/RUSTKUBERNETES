"use client";
import { useMemo, useState } from "react";
import { Button, IconButton } from "@/components/ui/Button";
import { Card, Row } from "@/components/ui/Card";
import { Badge, Empty, useToast } from "@/components/ui/Feedback";
import { Field, Input } from "@/components/ui/Form";
import { Icon } from "@/components/ui/icons";
import { Confirm, Modal } from "@/components/ui/Modal";
import { SimpleTable } from "@/components/ui/SimpleTable";
import { SegmentBar, StatusBadge } from "@/components/ui/Status";
import { kube } from "@/lib/api";
import { cn } from "@/lib/cn";
import { age, memory } from "@/lib/format";
import type { ClusterSnapshot } from "@/lib/types";
import { NamespaceFilter } from "./PodsTab";

const FAILING = ["Failed", "Error", "CrashLoopBackOff", "ImagePullBackOff", "ErrImagePull", "OOMKilled"];

export function OverviewTab({ s, onTab }: { cluster: string; s: ClusterSnapshot; onTab: (t: string) => void }) {
  const running = s.pods.filter((p) => p.status === "Running").length;
  const failing = s.pods.filter((p) => FAILING.includes(p.status)).length;
  const done = s.pods.filter((p) => ["Completed", "Succeeded"].includes(p.status)).length;
  const pending = s.pods.length - running - failing - done;
  const byNs = useMemo(() => {
    const m = new Map<string, number>();
    for (const p of s.pods) m.set(p.namespace, (m.get(p.namespace) ?? 0) + 1);
    return [...m.entries()].sort((a, b) => b[1] - a[1]);
  }, [s.pods]);
  return (
    <div className="grid gap-4 xl:grid-cols-2">
      <Card title="Pods by status" actions={<Button onClick={() => onTab("pods")}>All pods</Button>}>
        <SegmentBar total={s.pods.length} parts={[
          { value: running, className: "bg-success", label: "running" },
          { value: pending, className: "bg-warn", label: "pending" },
          { value: failing, className: "bg-danger", label: "failing" },
          { value: done, className: "bg-muted", label: "completed" },
        ]} />
        <div className="mt-3 flex flex-wrap gap-4 text-sm">
          <span><b className="text-success">{running}</b> running</span>
          <span><b className="text-warn">{pending}</b> pending</span>
          <span><b className="text-danger">{failing}</b> failing</span>
          <span><b className="text-muted">{done}</b> completed</span>
        </div>
        <div className="mt-4 space-y-1.5">
          {byNs.map(([ns, n]) => (
            <div key={ns} className="flex items-center gap-2 text-sm">
              <span className="w-36 truncate text-muted">{ns}</span>
              <div className="h-1.5 flex-1 overflow-hidden rounded-full bg-inset"><div className="h-full rounded-full bg-link" style={{ width: `${(n / Math.max(1, s.pods.length)) * 100}%` }} /></div>
              <span className="w-6 text-right tabular-nums">{n}</span>
            </div>
          ))}
        </div>
      </Card>
      {!s.restricted && (
        <Card title="Nodes" flush actions={<Button onClick={() => onTab("nodes")}>Details</Button>}>
          {s.nodes.length === 0 && <Empty>No nodes registered.</Empty>}
          {s.nodes.map((n) => (
            <Row key={n.name}>
              <Icon name="server" className="text-muted" />
              <b className="min-w-0 flex-1 truncate">{n.name}</b>
              <span className="hidden font-mono text-xs text-muted sm:inline">{n.ip}</span>
              <span className="text-xs text-muted">{s.pods.filter((p) => p.node === n.name).length} pods</span>
              <StatusBadge status={n.ready ? "ready" : "failed"} label={n.ready ? (n.unschedulable ? "Ready, cordoned" : "Ready") : "NotReady"} />
            </Row>
          ))}
        </Card>
      )}
      <Card title="Recent events" flush className="xl:col-span-2" actions={<Button onClick={() => onTab("events")}>All events</Button>}>
        {s.events.length === 0 && <Empty>No events.</Empty>}
        {s.events.slice(0, 8).map((e, i) => (
          <Row key={i}>
            <span className="w-32 shrink-0"><Badge tone={e.type === "Warning" ? "warn" : "neutral"}>{e.reason}</Badge></span>
            <span className="w-56 shrink-0 truncate text-xs text-muted">{e.object}</span>
            <span className="min-w-0 flex-1 truncate">{e.message}</span>
            <span className="text-xs text-muted">{age(e.time)}</span>
          </Row>
        ))}
      </Card>
    </div>
  );
}

export function ServicesTab({ s }: { s: ClusterSnapshot }) {
  const [ns, setNs] = useState("");
  const list = s.services.filter((x) => !ns || x.namespace === ns);
  return (
    <Card flush title="Services" actions={<NamespaceFilter s={s} value={ns} onChange={setNs} />}>
      {list.length === 0 ? <Empty>No services.</Empty> : (
        <SimpleTable head={["Name", "Namespace", "Type", "Cluster IP", "Ports", "Selector", "Endpoints", "Age"]}>
          {list.map((x) => {
            const sel = Object.entries(x.selector);
            const eps = sel.length ? s.pods.filter((p) => p.namespace === x.namespace && p.status === "Running").length : null;
            return (
              <tr key={x.namespace + "/" + x.name}>
                <td className="font-semibold">{x.name}</td>
                <td className="text-muted">{x.namespace}</td>
                <td><Badge tone={x.type === "NodePort" || x.type === "LoadBalancer" ? "info" : "neutral"}>{x.type}</Badge></td>
                <td className="font-mono text-xs">{x.clusterIP}</td>
                <td className="font-mono text-xs">{x.ports}</td>
                <td className="text-xs text-muted">{sel.map(([k, v]) => `${k}=${v}`).join(", ") || "–"}</td>
                <td className="text-xs text-muted">{eps === null ? "–" : `${x.name}.${x.namespace}.svc`}</td>
                <td className="text-muted">{age(x.created)}</td>
              </tr>
            );
          })}
        </SimpleTable>
      )}
    </Card>
  );
}

export function NodesTab({ cluster, s }: { cluster: string; s: ClusterSnapshot }) {
  const toast = useToast();
  const cordon = async (name: string, on: boolean) => {
    try {
      await kube(cluster, `/api/v1/nodes/${name}`, "PATCH", { spec: { unschedulable: on } }, "application/merge-patch+json");
      toast(on ? `${name} cordoned` : `${name} uncordoned`);
    } catch (e) { toast((e as Error).message, "error"); }
  };
  return (
    <div className="grid gap-4 xl:grid-cols-2">
      {s.nodes.map((n) => {
        const pods = s.pods.filter((p) => p.node === n.name);
        return (
          <Card key={n.name} title={<><Icon name="server" className="text-muted" />{n.name}</>} actions={<>
            <StatusBadge status={n.ready ? "ready" : "failed"} label={n.ready ? "Ready" : "NotReady"} />
            {n.unschedulable && <Badge tone="warn">cordoned</Badge>}
            <Button onClick={() => cordon(n.name, !n.unschedulable)}>{n.unschedulable ? "Uncordon" : "Cordon"}</Button>
          </>}>
            <dl className="grid grid-cols-[8rem_minmax(0,1fr)] gap-y-1 text-sm">
              <dt className="text-muted">Address</dt><dd className="font-mono">{n.ip}</dd>
              <dt className="text-muted">Capacity</dt><dd>{n.cpu} CPU · {memory(n.memory)} · {n.maxPods} pods</dd>
              <dt className="text-muted">Pods</dt><dd>{pods.length} ({pods.filter((p) => p.status === "Running").length} running)</dd>
              <dt className="text-muted">Kubelet</dt><dd>{n.version}</dd>
              <dt className="text-muted">Runtime</dt><dd>{n.runtime}</dd>
              <dt className="text-muted">OS</dt><dd className="truncate">{n.os} · {n.kernel}</dd>
              <dt className="text-muted">Age</dt><dd>{age(n.created)}</dd>
              {n.taints.length > 0 && <><dt className="text-muted">Taints</dt><dd className="text-xs">{n.taints.map((t) => `${t.key}:${t.effect}`).join(", ")}</dd></>}
            </dl>
          </Card>
        );
      })}
      {s.nodes.length === 0 && <Empty>No nodes registered.</Empty>}
    </div>
  );
}

export function NamespacesTab({ cluster, s }: { cluster: string; s: ClusterSnapshot }) {
  const toast = useToast();
  const [creating, setCreating] = useState(false);
  const [name, setName] = useState("");
  const [del, setDel] = useState<string | null>(null);
  const system = ["default", "kube-system", "kube-public", "kube-node-lease"];
  return (
    <>
      <Card flush title="Namespaces" actions={<Button variant="primary" icon="plus" onClick={() => { setName(""); setCreating(true); }}>New namespace</Button>}>
        <SimpleTable head={["Name", "Status", "Pods", "Deployments", "Services", "Age", ""]}>
          {s.namespaces.map((n) => (
            <tr key={n.name}>
              <td className="font-semibold">{n.name}</td>
              <td><StatusBadge status={n.phase || "Active"} /></td>
              <td className="tabular-nums">{s.pods.filter((p) => p.namespace === n.name).length}</td>
              <td className="tabular-nums">{s.deployments.filter((p) => p.namespace === n.name).length}</td>
              <td className="tabular-nums">{s.services.filter((p) => p.namespace === n.name).length}</td>
              <td className="text-muted">{age(n.created)}</td>
              <td className="text-right">{!system.includes(n.name) && <IconButton icon="trash" label="Delete namespace" variant="danger" onClick={() => setDel(n.name)} />}</td>
            </tr>
          ))}
        </SimpleTable>
      </Card>
      {creating && (
        <Modal title="New namespace" confirmLabel="Create" onClose={() => setCreating(false)} onConfirm={async () => {
          await kube(cluster, "/api/v1/namespaces", "POST", { apiVersion: "v1", kind: "Namespace", metadata: { name } });
          toast(`Namespace ${name} created`);
        }}>
          <Field label="Name"><Input value={name} onChange={(e) => setName(e.target.value.toLowerCase())} autoFocus /></Field>
        </Modal>
      )}
      {del && <Confirm title="Delete namespace" confirmLabel="Delete" message={<>Delete namespace <b>{del}</b> and everything in it?</>} onClose={() => setDel(null)}
        onConfirm={async () => { await kube(cluster, `/api/v1/namespaces/${del}`, "DELETE"); toast(`Deleting ${del}`); }} />}
    </>
  );
}

export function EventsTab({ s }: { s: ClusterSnapshot }) {
  const [ns, setNs] = useState("");
  const [warnings, setWarnings] = useState(false);
  const list = s.events.filter((e) => (!ns || e.namespace === ns) && (!warnings || e.type === "Warning"));
  return (
    <Card flush title="Events" actions={<>
      <NamespaceFilter s={s} value={ns} onChange={setNs} />
      <Button className={cn(warnings && "!border-warn !text-warn")} onClick={() => setWarnings(!warnings)}>Warnings only</Button>
    </>}>
      {list.length === 0 ? <Empty>No events.</Empty> : (
        <SimpleTable head={["Last seen", "Type", "Reason", "Object", "Message", "Count"]}>
          {list.map((e, i) => (
            <tr key={i}>
              <td className="whitespace-nowrap text-muted">{age(e.time)}</td>
              <td><Badge tone={e.type === "Warning" ? "warn" : "neutral"}>{e.type}</Badge></td>
              <td className="font-semibold">{e.reason}</td>
              <td className="whitespace-nowrap text-xs">{e.namespace && <span className="text-muted">{e.namespace}/</span>}{e.object}</td>
              <td className="min-w-[16rem]">{e.message}</td>
              <td className="tabular-nums">{e.count}</td>
            </tr>
          ))}
        </SimpleTable>
      )}
    </Card>
  );
}
