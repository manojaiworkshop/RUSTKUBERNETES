"use client";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Button, IconButton } from "@/components/ui/Button";
import { Card } from "@/components/ui/Card";
import { Alert, Empty, useToast } from "@/components/ui/Feedback";
import { Checkbox, Input, Select } from "@/components/ui/Form";
import { Confirm, Modal } from "@/components/ui/Modal";
import { SimpleTable } from "@/components/ui/SimpleTable";
import { StatusBadge } from "@/components/ui/Status";
import { kube, kubeText, qs } from "@/lib/api";
import { age } from "@/lib/format";
import type { ClusterSnapshot, PodRow } from "@/lib/types";

/** Namespace picker shared by the list tabs. */
export function NamespaceFilter({ s, value, onChange }: { s: ClusterSnapshot; value: string; onChange: (v: string) => void }) {
  return (
    <Select value={value} onChange={(e) => onChange(e.target.value)} className="!w-48" aria-label="Namespace">
      <option value="">All namespaces</option>
      {s.namespaces.map((n) => <option key={n.name} value={n.name}>{n.name}</option>)}
    </Select>
  );
}

function LogViewer({ cluster, pod, onClose }: { cluster: string; pod: PodRow; onClose: () => void }) {
  const [container, setContainer] = useState(pod.containers[0]?.name ?? "");
  const [follow, setFollow] = useState(true);
  const [previous, setPrevious] = useState(false);
  const [text, setText] = useState("");
  const [error, setError] = useState("");
  const ref = useRef<HTMLPreElement>(null);
  const load = useCallback(async () => {
    try {
      const t = await kubeText(cluster, `/api/v1/namespaces/${pod.namespace}/pods/${pod.name}/log?${qs({ container, tailLines: 1000, timestamps: false, previous })}`);
      setText(t);
      setError("");
    } catch (e) {
      setError((e as Error).message);
    }
  }, [cluster, pod, container, previous]);
  useEffect(() => { load(); }, [load]);
  useEffect(() => {
    if (!follow) return;
    const t = setInterval(load, 2000);
    return () => clearInterval(t);
  }, [follow, load]);
  useEffect(() => { if (ref.current && follow) ref.current.scrollTop = ref.current.scrollHeight; }, [text, follow]);
  return (
    <Modal title={<>Logs · {pod.namespace}/{pod.name}</>} wide onClose={onClose}>
      <div className="flex flex-wrap items-center gap-3">
        {pod.containers.length > 1 && (
          <Select value={container} onChange={(e) => setContainer(e.target.value)} className="!w-48">{pod.containers.map((c) => <option key={c.name}>{c.name}</option>)}</Select>
        )}
        <Checkbox label="Follow" checked={follow} onChange={(e) => setFollow(e.target.checked)} />
        <Checkbox label="Previous container" checked={previous} onChange={(e) => setPrevious(e.target.checked)} />
        <Button icon="sync" onClick={load}>Refresh</Button>
      </div>
      {error && <Alert className="!mb-0">{error}</Alert>}
      <pre ref={ref} className="m-0 h-[55dvh] overflow-auto rounded-md border border-line bg-[#0d1117] p-3 font-mono text-[12px] leading-relaxed text-[#e6edf3]">{text || (error ? "" : "(no output)")}</pre>
    </Modal>
  );
}

export function PodsTab({ cluster, s }: { cluster: string; s: ClusterSnapshot }) {
  const toast = useToast();
  const [ns, setNs] = useState("");
  const [q, setQ] = useState("");
  const [logs, setLogs] = useState<PodRow | null>(null);
  const [del, setDel] = useState<PodRow | null>(null);
  const pods = useMemo(() => s.pods.filter((p) => (!ns || p.namespace === ns) && (!q || p.name.includes(q) || p.node.includes(q))), [s.pods, ns, q]);
  return (
    <>
      <Card flush title={<>Pods <span className="text-xs font-normal text-muted">live</span></>} actions={<>
        <NamespaceFilter s={s} value={ns} onChange={setNs} />
        <Input value={q} onChange={(e) => setQ(e.target.value)} placeholder="Filter by name or node…" className="!w-56" />
      </>}>
        {pods.length === 0 ? <Empty>No pods{ns && <> in {ns}</>}.</Empty> : (
          <SimpleTable head={["Name", "Namespace", "Ready", "Status", "Restarts", "Node", "IP", "Age", ""]}>
            {pods.map((p) => (
              <tr key={p.namespace + "/" + p.name}>
                <td className="max-w-[18rem] truncate font-semibold" title={p.containers.map((c) => c.image).join(", ")}>{p.name}</td>
                <td className="text-muted">{p.namespace}</td>
                <td className="tabular-nums">{p.ready}</td>
                <td><StatusBadge status={p.status} /></td>
                <td className={p.restarts > 0 ? "tabular-nums text-warn" : "tabular-nums"}>{p.restarts}</td>
                <td className="text-muted">{p.node || "–"}</td>
                <td className="font-mono text-xs">{p.ip || "–"}</td>
                <td className="text-muted">{age(p.created)}</td>
                <td className="whitespace-nowrap text-right">
                  <span className="inline-flex gap-1">
                    <IconButton icon="logs" label="Logs" onClick={() => setLogs(p)} />
                    <IconButton icon="trash" label="Delete pod" variant="danger" onClick={() => setDel(p)} />
                  </span>
                </td>
              </tr>
            ))}
          </SimpleTable>
        )}
      </Card>
      {logs && <LogViewer cluster={cluster} pod={logs} onClose={() => setLogs(null)} />}
      {del && <Confirm title="Delete pod" confirmLabel="Delete pod" message={<>Delete pod <b>{del.namespace}/{del.name}</b>? A controller (Deployment, Job…) replaces it.</>} onClose={() => setDel(null)}
        onConfirm={async () => { await kube(cluster, `/api/v1/namespaces/${del.namespace}/pods/${del.name}`, "DELETE"); toast("Pod deleted"); }} />}
    </>
  );
}
