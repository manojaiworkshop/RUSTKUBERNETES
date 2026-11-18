"use client";
import { useEffect, useState } from "react";
import { Button, IconButton } from "@/components/ui/Button";
import { Card } from "@/components/ui/Card";
import { Alert, Badge, Empty, Loading, useToast } from "@/components/ui/Feedback";
import { Checkbox, Field, Input, Select } from "@/components/ui/Form";
import { Confirm, Modal } from "@/components/ui/Modal";
import { SimpleTable } from "@/components/ui/SimpleTable";
import { api, kube } from "@/lib/api";
import { ROLE_LABEL } from "@/lib/format";
import { useSession } from "@/lib/session";
import type { ClusterSnapshot, Grant, User } from "@/lib/types";
import { useAsync } from "@/lib/useAsync";

interface Binding { kind: "ClusterRoleBinding" | "RoleBinding"; metadata: { name: string; namespace?: string; labels?: Record<string, string> }; roleRef: { kind: string; name: string }; subjects?: { kind: string; name: string; namespace?: string }[] }
interface List<T> { items: T[] }

/** Console users' access to this cluster (turned into RBAC bindings by rustkubeadmin). */
function ConsoleAccess({ cluster, s }: { cluster: string; s: ClusterSnapshot }) {
  const { users } = useSession();
  const toast = useToast();
  const [adding, setAdding] = useState<{ user: string; namespace: string; role: Grant["role"] } | null>(null);
  const [removing, setRemoving] = useState<{ user: User; grant: Grant } | null>(null);
  const rows = users.flatMap((u) => u.grants.filter((g) => g.cluster === cluster).map((g) => ({ u, g })));
  const save = (u: User, grants: Grant[]) => api("/api/users/update", { username: u.username, permissions: u.permissions, grants, disabled: u.disabled });
  return (
    <Card flush title="Console users with access" actions={<Button variant="primary" icon="plus" disabled={users.length === 0} onClick={() => setAdding({ user: users[0]?.username ?? "", namespace: "", role: "view" })}>Grant access</Button>}>
      {users.length === 0 && <Empty>No console users yet — create them under Users &amp; access.</Empty>}
      {users.length > 0 && rows.length === 0 && <Empty>No console user has access to this cluster yet.</Empty>}
      {rows.length > 0 && (
        <SimpleTable head={["User", "Scope", "Role", "Kubernetes identity", ""]}>
          {rows.map(({ u, g }) => (
            <tr key={u.username + g.namespace + g.role}>
              <td className="font-semibold">{u.username}{u.disabled && <Badge tone="danger" className="ml-2">disabled</Badge>}</td>
              <td>{g.namespace ? <>namespace <b>{g.namespace}</b></> : <Badge tone="warn">whole cluster</Badge>}</td>
              <td><Badge tone={g.role === "admin" ? "warn" : g.role === "edit" ? "info" : "neutral"}>{ROLE_LABEL[g.role]}</Badge></td>
              <td className="font-mono text-xs text-muted">rustkubeadmin:{u.username}</td>
              <td className="text-right"><IconButton icon="trash" label="Remove access" variant="danger" onClick={() => setRemoving({ user: u, grant: g })} /></td>
            </tr>
          ))}
        </SimpleTable>
      )}
      {adding && (
        <Modal title="Grant access" confirmLabel="Grant" onClose={() => setAdding(null)} onConfirm={async () => {
          const u = users.find((x) => x.username === adding.user);
          if (!u) throw new Error("Pick a user");
          await save(u, [...u.grants.filter((g) => !(g.cluster === cluster && g.namespace === adding.namespace)), { cluster, namespace: adding.namespace, role: adding.role }]);
          toast(`Access granted to ${u.username}`);
        }}>
          <Field label="User"><Select value={adding.user} onChange={(e) => setAdding({ ...adding, user: e.target.value })}>{users.map((u) => <option key={u.username}>{u.username}</option>)}</Select></Field>
          <Field label="Scope" hint="a namespace that does not exist yet is created">
            <Select value={adding.namespace === "" ? "" : "__ns"} onChange={(e) => setAdding({ ...adding, namespace: e.target.value === "" ? "" : s.namespaces.find((n) => n.name !== "kube-system")?.name ?? "default" })}>
              <option value="">Whole cluster</option><option value="__ns">One namespace</option>
            </Select>
          </Field>
          {adding.namespace !== "" && <Field label="Namespace"><Input list="ns-list" value={adding.namespace} onChange={(e) => setAdding({ ...adding, namespace: e.target.value.toLowerCase() })} /><datalist id="ns-list">{s.namespaces.map((n) => <option key={n.name} value={n.name} />)}</datalist></Field>}
          <Field label="Role">
            <Select value={adding.role} onChange={(e) => setAdding({ ...adding, role: e.target.value as Grant["role"] })}>
              <option value="view">View — read everything except Secrets</option>
              <option value="edit">Edit — create and change workloads, Secrets, Services</option>
              <option value="admin">Admin — {adding.namespace ? "full control of the namespace incl. RBAC" : "cluster-admin: everything"}</option>
            </Select>
          </Field>
        </Modal>
      )}
      {removing && <Confirm title="Remove access" confirmLabel="Remove" message={<>Remove <b>{removing.user.username}</b>&apos;s {ROLE_LABEL[removing.grant.role]} access {removing.grant.namespace ? <>to namespace <b>{removing.grant.namespace}</b></> : "to the whole cluster"}?</>}
        onClose={() => setRemoving(null)} onConfirm={async () => {
          await save(removing.user, removing.user.grants.filter((g) => g !== removing.grant));
          toast("Access removed");
        }} />}
    </Card>
  );
}

/** Kubernetes RBAC bindings, read and changed as the signed-in user. */
function Bindings({ cluster, s }: { cluster: string; s: ClusterSnapshot }) {
  const toast = useToast();
  const [showSystem, setShowSystem] = useState(false);
  const { data, error, loading, reload } = useAsync(async () => {
    const [crb, rb, cr] = await Promise.all([
      kube<List<Binding>>(cluster, "/apis/rbac.authorization.k8s.io/v1/clusterrolebindings").catch(() => ({ items: [] as Binding[] })),
      kube<List<Binding>>(cluster, "/apis/rbac.authorization.k8s.io/v1/rolebindings"),
      kube<List<{ metadata: { name: string } }>>(cluster, "/apis/rbac.authorization.k8s.io/v1/clusterroles").catch(() => ({ items: [] })),
    ]);
    return {
      bindings: [...crb.items.map((b) => ({ ...b, kind: "ClusterRoleBinding" as const })), ...rb.items.map((b) => ({ ...b, kind: "RoleBinding" as const }))],
      roles: cr.items.map((r) => r.metadata.name).filter((n) => !n.startsWith("system:")),
    };
  }, [cluster]);
  const [draft, setDraft] = useState<{ scope: string; name: string; role: string; subjectKind: string; subject: string; saNamespace: string } | null>(null);
  const [del, setDel] = useState<Binding | null>(null);
  useEffect(() => { const t = setInterval(reload, 15000); return () => clearInterval(t); }, [reload]);
  const visible = (data?.bindings ?? []).filter((b) => showSystem || !(b.metadata.name.startsWith("system:") || b.metadata.labels?.["kubernetes.io/bootstrapping"]));
  const path = (b: Binding) => b.kind === "ClusterRoleBinding" ? `/apis/rbac.authorization.k8s.io/v1/clusterrolebindings/${b.metadata.name}` : `/apis/rbac.authorization.k8s.io/v1/namespaces/${b.metadata.namespace}/rolebindings/${b.metadata.name}`;
  return (
    <Card flush title="Kubernetes RBAC bindings" actions={<>
      <Checkbox label="Show system bindings" checked={showSystem} onChange={(e) => setShowSystem(e.target.checked)} />
      <Button icon="sync" onClick={reload}>Refresh</Button>
      <Button variant="primary" icon="plus" onClick={() => setDraft({ scope: "", name: "", role: "view", subjectKind: "User", subject: "", saNamespace: "default" })}>New binding</Button>
    </>}>
      <Loading error={error} loading={loading}>
        {data && (visible.length === 0 ? <Empty>No bindings.</Empty> : (
          <SimpleTable head={["Binding", "Scope", "Role", "Subjects", ""]}>
            {visible.map((b) => (
              <tr key={b.kind + (b.metadata.namespace ?? "") + b.metadata.name}>
                <td className="font-semibold">{b.metadata.name}{b.metadata.labels?.["rustkubeadmin.io/managed"] && <Badge tone="info" className="ml-2">console</Badge>}</td>
                <td>{b.kind === "ClusterRoleBinding" ? <Badge tone="warn">cluster</Badge> : <>ns <b>{b.metadata.namespace}</b></>}</td>
                <td className="text-xs">{b.roleRef.kind}/<b>{b.roleRef.name}</b></td>
                <td className="text-xs">{(b.subjects ?? []).map((x) => `${x.kind}:${x.namespace ? x.namespace + "/" : ""}${x.name}`).join(", ") || "–"}</td>
                <td className="text-right">{!b.metadata.labels?.["rustkubeadmin.io/managed"] && <IconButton icon="trash" label="Delete binding" variant="danger" onClick={() => setDel(b)} />}</td>
              </tr>
            ))}
          </SimpleTable>
        ))}
      </Loading>
      {draft && (
        <Modal title="New RBAC binding" confirmLabel="Create" onClose={() => setDraft(null)} onConfirm={async () => {
          const subject = draft.subjectKind === "ServiceAccount"
            ? { kind: "ServiceAccount", name: draft.subject, namespace: draft.saNamespace }
            : { kind: draft.subjectKind, apiGroup: "rbac.authorization.k8s.io", name: draft.subject };
          const body = {
            apiVersion: "rbac.authorization.k8s.io/v1",
            kind: draft.scope ? "RoleBinding" : "ClusterRoleBinding",
            metadata: { name: draft.name, ...(draft.scope ? { namespace: draft.scope } : {}) },
            roleRef: { apiGroup: "rbac.authorization.k8s.io", kind: "ClusterRole", name: draft.role },
            subjects: [subject],
          };
          await kube(cluster, draft.scope ? `/apis/rbac.authorization.k8s.io/v1/namespaces/${draft.scope}/rolebindings` : "/apis/rbac.authorization.k8s.io/v1/clusterrolebindings", "POST", body);
          toast("Binding created");
          reload();
        }}>
          <Field label="Binding name"><Input value={draft.name} onChange={(e) => setDraft({ ...draft, name: e.target.value })} autoFocus required /></Field>
          <div className="grid grid-cols-2 gap-3">
            <Field label="Scope">
              <Select value={draft.scope} onChange={(e) => setDraft({ ...draft, scope: e.target.value })}>
                <option value="">Whole cluster (ClusterRoleBinding)</option>
                {s.namespaces.map((n) => <option key={n.name} value={n.name}>namespace {n.name}</option>)}
              </Select>
            </Field>
            <Field label="ClusterRole"><Select value={draft.role} onChange={(e) => setDraft({ ...draft, role: e.target.value })}>{(data?.roles ?? ["view", "edit", "admin", "cluster-admin"]).map((r) => <option key={r}>{r}</option>)}</Select></Field>
          </div>
          <div className="grid grid-cols-[10rem_minmax(0,1fr)] gap-3">
            <Field label="Subject"><Select value={draft.subjectKind} onChange={(e) => setDraft({ ...draft, subjectKind: e.target.value })}><option>User</option><option>Group</option><option>ServiceAccount</option></Select></Field>
            <Field label="Name"><Input value={draft.subject} onChange={(e) => setDraft({ ...draft, subject: e.target.value })} placeholder={draft.subjectKind === "User" ? "e.g. rustkubeadmin:jane or a certificate CN" : ""} required /></Field>
          </div>
          {draft.subjectKind === "ServiceAccount" && <Field label="ServiceAccount namespace"><Select value={draft.saNamespace} onChange={(e) => setDraft({ ...draft, saNamespace: e.target.value })}>{s.namespaces.map((n) => <option key={n.name}>{n.name}</option>)}</Select></Field>}
          <p className="text-xs text-muted">Kubernetes refuses bindings that grant permissions you do not hold yourself (escalation prevention).</p>
        </Modal>
      )}
      {del && <Confirm title="Delete binding" confirmLabel="Delete" message={<>Delete {del.kind} <b>{del.metadata.name}</b>?</>} onClose={() => setDel(null)}
        onConfirm={async () => { await kube(cluster, path(del), "DELETE"); toast("Binding deleted"); reload(); }} />}
    </Card>
  );
}

/** What the signed-in user may do in a namespace (`kubectl auth can-i --list`). */
function MyRules({ cluster, s }: { cluster: string; s: ClusterSnapshot }) {
  const [ns, setNs] = useState(s.namespaces[0]?.name ?? "default");
  const { data, error, loading } = useAsync(async () => {
    const r = await kube<{ status: { resourceRules: { verbs: string[]; apiGroups?: string[]; resources?: string[] }[] } }>(cluster, "/apis/authorization.k8s.io/v1/selfsubjectrulesreviews", "POST",
      { apiVersion: "authorization.k8s.io/v1", kind: "SelfSubjectRulesReview", spec: { namespace: ns } });
    return r.status.resourceRules;
  }, [cluster, ns]);
  return (
    <Card flush title="Your permissions" actions={<Select className="!w-48" value={ns} onChange={(e) => setNs(e.target.value)}>{s.namespaces.map((n) => <option key={n.name}>{n.name}</option>)}</Select>}>
      <Loading error={error} loading={loading}>
        {data && (data.length === 0 ? <Empty>No permissions in {ns}.</Empty> : (
          <SimpleTable head={["Resources", "API groups", "Verbs"]}>
            {data.slice(0, 40).map((r, i) => (
              <tr key={i}><td className="text-xs">{(r.resources ?? []).join(", ")}</td><td className="text-xs text-muted">{(r.apiGroups ?? []).map((g) => g || "core").join(", ")}</td><td className="text-xs">{r.verbs.join(", ")}</td></tr>
            ))}
          </SimpleTable>
        ))}
      </Loading>
    </Card>
  );
}

export function AccessTab({ cluster, s }: { cluster: string; s: ClusterSnapshot }) {
  const { can } = useSession();
  return (
    <>
      {can("users.manage") ? <ConsoleAccess cluster={cluster} s={s} /> : <Alert tone="info">Ask an administrator for more access: grants are managed under Users &amp; access.</Alert>}
      <Bindings cluster={cluster} s={s} />
      <MyRules cluster={cluster} s={s} />
    </>
  );
}
