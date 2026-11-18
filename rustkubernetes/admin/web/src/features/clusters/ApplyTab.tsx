"use client";
import { useState } from "react";
import { Button } from "@/components/ui/Button";
import { Card } from "@/components/ui/Card";
import { Alert, useToast } from "@/components/ui/Feedback";
import { Select, Textarea } from "@/components/ui/Form";
import { api } from "@/lib/api";
import type { ClusterSnapshot } from "@/lib/types";

const TEMPLATES: Record<string, string> = {
  Deployment: `apiVersion: apps/v1
kind: Deployment
metadata:
  name: web
spec:
  replicas: 2
  selector:
    matchLabels: {app: web}
  template:
    metadata:
      labels: {app: web}
    spec:
      containers:
        - name: web
          image: nginx:1.27-alpine
          ports: [{containerPort: 80}]
---
apiVersion: v1
kind: Service
metadata:
  name: web
spec:
  selector: {app: web}
  ports: [{port: 80}]
`,
  ConfigMap: `apiVersion: v1
kind: ConfigMap
metadata:
  name: settings
data:
  greeting: hello
`,
  Job: `apiVersion: batch/v1
kind: Job
metadata:
  name: hello
spec:
  template:
    spec:
      restartPolicy: Never
      containers:
        - name: hello
          image: busybox:1.36
          command: [sh, -c, "echo hello from the cluster"]
`,
  "Role + RoleBinding": `apiVersion: rbac.authorization.k8s.io/v1
kind: Role
metadata:
  name: pod-reader
rules:
  - apiGroups: [""]
    resources: [pods, pods/log]
    verbs: [get, list, watch]
---
apiVersion: rbac.authorization.k8s.io/v1
kind: RoleBinding
metadata:
  name: read-pods
roleRef: {apiGroup: rbac.authorization.k8s.io, kind: Role, name: pod-reader}
subjects:
  - {apiGroup: rbac.authorization.k8s.io, kind: User, name: jane}
`,
};

export function ApplyTab({ cluster, s }: { cluster: string; s: ClusterSnapshot }) {
  const toast = useToast();
  const [ns, setNs] = useState(s.namespaces.find((n) => n.name === "default")?.name ?? s.namespaces[0]?.name ?? "default");
  const [yaml, setYaml] = useState(TEMPLATES.Deployment);
  const [results, setResults] = useState<string[] | null>(null);
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  const apply = async () => {
    setBusy(true);
    setError("");
    setResults(null);
    try {
      const r = await api<{ results: string[] }>("/api/clusters/apply", { name: cluster, namespace: ns, yaml });
      setResults(r.results);
      toast(`Applied ${r.results.length} object(s)`);
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setBusy(false);
    }
  };
  return (
    <Card title="Apply YAML" actions={<>
      <Select className="!w-48" value="" onChange={(e) => e.target.value && setYaml(TEMPLATES[e.target.value])} aria-label="Template">
        <option value="">Insert a template…</option>
        {Object.keys(TEMPLATES).map((t) => <option key={t}>{t}</option>)}
      </Select>
      <Select className="!w-44" value={ns} onChange={(e) => setNs(e.target.value)} aria-label="Default namespace">
        {s.namespaces.map((n) => <option key={n.name}>{n.name}</option>)}
      </Select>
      <Button variant="primary" icon="upload" disabled={busy} onClick={apply}>{busy ? "Applying…" : "Apply"}</Button>
    </>}>
      <p className="mb-3 text-sm text-muted">Server-side apply, like <code>kubectl apply --server-side</code>: objects are created or updated; several documents may be separated by <code>---</code>. Objects without a namespace go to <b>{ns}</b>. Your RBAC permissions apply.</p>
      {error && <Alert>{error}</Alert>}
      {results && <Alert tone="success">{results.map((r) => <div key={r}>✔ {r}</div>)}</Alert>}
      <Textarea rows={22} spellCheck={false} className="font-mono text-xs leading-relaxed" value={yaml} onChange={(e) => setYaml(e.target.value)}
        onKeyDown={(e) => { if ((e.ctrlKey || e.metaKey) && e.key === "Enter") apply(); }} />
      <p className="mt-2 text-xs text-muted">Ctrl+Enter applies.</p>
    </Card>
  );
}
