"use client";
import { useMemo, useState } from "react";
import { Button, IconButton } from "@/components/ui/Button";
import { Card } from "@/components/ui/Card";
import { Empty, Progress, useToast } from "@/components/ui/Feedback";
import { Field, Input, Select } from "@/components/ui/Form";
import { Confirm, Modal } from "@/components/ui/Modal";
import { SimpleTable } from "@/components/ui/SimpleTable";
import { api, kube } from "@/lib/api";
import { age } from "@/lib/format";
import type { ClusterSnapshot, DeploymentRow } from "@/lib/types";
import { NamespaceFilter } from "./PodsTab";

interface Draft { name: string; image: string; replicas: number; namespace: string; port: string; expose: "" | "ClusterIP" | "NodePort" }

function deploymentYaml(d: Draft): string {
  const port = d.port ? `\n          ports: [{containerPort: ${Number(d.port)}}]` : "";
  let y = `apiVersion: apps/v1
kind: Deployment
metadata: {name: ${d.name}, namespace: ${d.namespace}, labels: {app: ${d.name}}}
spec:
  replicas: ${d.replicas}
  selector: {matchLabels: {app: ${d.name}}}
  template:
    metadata: {labels: {app: ${d.name}}}
    spec:
      containers:
        - name: ${d.name}
          image: ${d.image}${port}
`;
  if (d.expose && d.port) {
    y += `---
apiVersion: v1
kind: Service
metadata: {name: ${d.name}, namespace: ${d.namespace}}
spec:
  type: ${d.expose}
  selector: {app: ${d.name}}
  ports: [{port: ${Number(d.port)}, targetPort: ${Number(d.port)}}]
`;
  }
  return y;
}

export function WorkloadsTab({ cluster, s }: { cluster: string; s: ClusterSnapshot }) {
  const toast = useToast();
  const [ns, setNs] = useState("");
  const [draft, setDraft] = useState<Draft | null>(null);
  const [scale, setScale] = useState<{ d: DeploymentRow; n: number } | null>(null);
  const [del, setDel] = useState<DeploymentRow | null>(null);
  const list = useMemo(() => s.deployments.filter((d) => !ns || d.namespace === ns), [s.deployments, ns]);
  const path = (d: DeploymentRow) => `/apis/apps/v1/namespaces/${d.namespace}/deployments/${d.name}`;
  const patch = (d: DeploymentRow, body: unknown) => kube(cluster, path(d), "PATCH", body, "application/merge-patch+json");
  return (
    <>
      <Card flush title="Deployments" actions={<>
        <NamespaceFilter s={s} value={ns} onChange={setNs} />
        <Button variant="primary" icon="plus" onClick={() => setDraft({ name: "", image: "nginx:1.27-alpine", replicas: 2, namespace: ns || s.namespaces.find((n) => n.name === "default")?.name || s.namespaces[0]?.name || "default", port: "80", expose: "ClusterIP" })}>Deploy app</Button>
      </>}>
        {list.length === 0 ? <Empty>No deployments{ns && <> in {ns}</>}.</Empty> : (
          <SimpleTable head={["Name", "Namespace", "Ready", "", "Up-to-date", "Images", "Age", ""]}>
            {list.map((d) => (
              <tr key={d.namespace + "/" + d.name}>
                <td className="font-semibold">{d.name}</td>
                <td className="text-muted">{d.namespace}</td>
                <td className="tabular-nums">{d.ready}/{d.replicas}</td>
                <td className="w-32"><Progress value={d.replicas ? (d.ready / d.replicas) * 100 : 100} tone={d.ready === d.replicas ? "success" : "info"} /></td>
                <td className="tabular-nums">{d.updated}</td>
                <td className="max-w-[16rem] truncate font-mono text-xs">{d.images.join(", ")}</td>
                <td className="text-muted">{age(d.created)}</td>
                <td className="whitespace-nowrap text-right">
                  <span className="inline-flex gap-1">
                    <IconButton icon="scale" label="Scale" onClick={() => setScale({ d, n: d.replicas })} />
                    <IconButton icon="sync" label="Restart (rolling)" onClick={async () => {
                      try {
                        await patch(d, { spec: { template: { metadata: { annotations: { "kubectl.kubernetes.io/restartedAt": new Date().toISOString() } } } } });
                        toast(`Restarting ${d.name}`);
                      } catch (e) { toast((e as Error).message, "error"); }
                    }} />
                    <IconButton icon="trash" label="Delete" variant="danger" onClick={() => setDel(d)} />
                  </span>
                </td>
              </tr>
            ))}
          </SimpleTable>
        )}
      </Card>
      {draft && (
        <Modal title="Deploy an app" confirmLabel="Deploy" onClose={() => setDraft(null)} onConfirm={async () => {
          if (!/^[a-z0-9]([a-z0-9-]*[a-z0-9])?$/.test(draft.name)) throw new Error("The name must be lowercase letters, digits and -");
          const r = await api<{ results: string[] }>("/api/clusters/apply", { name: cluster, namespace: draft.namespace, yaml: deploymentYaml(draft) });
          toast(r.results.join(" · "));
        }}>
          <div className="grid grid-cols-2 gap-3">
            <Field label="Name"><Input value={draft.name} onChange={(e) => setDraft({ ...draft, name: e.target.value.toLowerCase() })} autoFocus required /></Field>
            <Field label="Namespace">
              <Select value={draft.namespace} onChange={(e) => setDraft({ ...draft, namespace: e.target.value })}>{s.namespaces.map((n) => <option key={n.name}>{n.name}</option>)}</Select>
            </Field>
          </div>
          <Field label="Container image" hint="must be pullable by the nodes (or already on them)"><Input value={draft.image} onChange={(e) => setDraft({ ...draft, image: e.target.value })} required /></Field>
          <div className="grid grid-cols-3 gap-3">
            <Field label="Replicas"><Input type="number" min={0} max={100} value={draft.replicas} onChange={(e) => setDraft({ ...draft, replicas: Number(e.target.value) })} /></Field>
            <Field label="Port" hint="optional"><Input value={draft.port} onChange={(e) => setDraft({ ...draft, port: e.target.value.replace(/\D/g, "") })} /></Field>
            <Field label="Expose as">
              <Select value={draft.expose} onChange={(e) => setDraft({ ...draft, expose: e.target.value as Draft["expose"] })}>
                <option value="">no Service</option><option value="ClusterIP">ClusterIP</option><option value="NodePort">NodePort</option>
              </Select>
            </Field>
          </div>
          <details><summary className="cursor-pointer text-sm text-link">YAML</summary><pre className="mt-2 overflow-auto rounded-md border border-line bg-canvas p-3 font-mono text-xs">{deploymentYaml(draft)}</pre></details>
        </Modal>
      )}
      {scale && (
        <Modal title={`Scale ${scale.d.namespace}/${scale.d.name}`} confirmLabel="Scale" onClose={() => setScale(null)} onConfirm={async () => {
          await patch(scale.d, { spec: { replicas: scale.n } });
          toast(`Scaled ${scale.d.name} to ${scale.n}`);
        }}>
          <Field label="Replicas"><Input type="number" min={0} max={500} value={scale.n} onChange={(e) => setScale({ ...scale, n: Number(e.target.value) })} autoFocus /></Field>
        </Modal>
      )}
      {del && <Confirm title="Delete deployment" confirmLabel="Delete" message={<>Delete <b>{del.namespace}/{del.name}</b> and its pods?</>} onClose={() => setDel(null)}
        onConfirm={async () => { await kube(cluster, path(del), "DELETE"); toast("Deployment deleted"); }} />}
    </>
  );
}
