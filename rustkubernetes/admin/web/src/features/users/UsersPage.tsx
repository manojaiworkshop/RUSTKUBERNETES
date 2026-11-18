"use client";
import { useState } from "react";
import { Button, IconButton } from "@/components/ui/Button";
import { Card, PageHeader } from "@/components/ui/Card";
import { Alert, Badge, Empty, useToast } from "@/components/ui/Feedback";
import { Checkbox, Field, Input, Select } from "@/components/ui/Form";
import { Icon } from "@/components/ui/icons";
import { Confirm, Modal } from "@/components/ui/Modal";
import { SimpleTable } from "@/components/ui/SimpleTable";
import { api } from "@/lib/api";
import { age, ROLE_LABEL } from "@/lib/format";
import { useSession } from "@/lib/session";
import type { Grant, User } from "@/lib/types";

interface Draft { existing: boolean; username: string; password: string; permissions: string[]; grants: Grant[]; disabled: boolean }

function UserModal({ draft, onClose }: { draft: Draft; onClose: () => void }) {
  const { meta, clusters, me } = useSession();
  const toast = useToast();
  const [d, setD] = useState(draft);
  const set = (p: Partial<Draft>) => setD({ ...d, ...p });
  const setGrant = (i: number, g: Partial<Grant>) => set({ grants: d.grants.map((x, j) => (j === i ? { ...x, ...g } : x)) });
  return (
    <Modal title={d.existing ? `Edit ${d.username}` : "New user"} confirmLabel={d.existing ? "Save" : "Create user"} wide onClose={onClose} onConfirm={async () => {
      await api(d.existing ? "/api/users/update" : "/api/users", d);
      toast(d.existing ? "User updated" : `User ${d.username} created`);
    }}>
      <div className="grid grid-cols-2 gap-3">
        <Field label="Username" hint="lowercase, digits, . _ -"><Input value={d.username} disabled={d.existing} onChange={(e) => set({ username: e.target.value.toLowerCase() })} autoFocus required /></Field>
        <Field label="Password" hint={d.existing ? "leave empty to keep" : "at least 8 characters"}><Input type="password" autoComplete="new-password" value={d.password} onChange={(e) => set({ password: e.target.value })} required={!d.existing} /></Field>
      </div>
      <fieldset className="rounded-md border border-line p-3">
        <legend className="px-1 text-sm font-semibold">Console permissions</legend>
        <div className="space-y-2">
          {meta.permissions.map((p) => (
            <Checkbox key={p.id} disabled={!me.superadmin && !me.permissions.includes(p.id)} checked={d.permissions.includes(p.id)}
              onChange={(e) => set({ permissions: e.target.checked ? [...d.permissions, p.id] : d.permissions.filter((x) => x !== p.id) })}
              label={<><b className="font-mono text-xs">{p.id}</b> <span className="text-muted">— {p.description}</span></>} />
          ))}
        </div>
      </fieldset>
      <fieldset className="rounded-md border border-line p-3">
        <legend className="px-1 text-sm font-semibold">Cluster access</legend>
        <p className="mb-2 text-xs text-muted">Each grant becomes a Kubernetes RoleBinding (one namespace) or ClusterRoleBinding (whole cluster) for <code>rustkubeadmin:{d.username || "user"}</code>.</p>
        {d.grants.map((g, i) => (
          <div key={i} className="mb-2 grid grid-cols-[minmax(0,1fr)_minmax(0,1fr)_8rem_auto] items-end gap-2">
            <Field label="Cluster"><Select value={g.cluster} onChange={(e) => setGrant(i, { cluster: e.target.value })}>{clusters.map((c) => <option key={c.name}>{c.name}</option>)}</Select></Field>
            <Field label="Namespace" hint="empty: whole cluster"><Input value={g.namespace} onChange={(e) => setGrant(i, { namespace: e.target.value.toLowerCase() })} placeholder="all namespaces" /></Field>
            <Field label="Role"><Select value={g.role} onChange={(e) => setGrant(i, { role: e.target.value as Grant["role"] })}><option value="view">View</option><option value="edit">Edit</option><option value="admin">Admin</option></Select></Field>
            <IconButton icon="trash" label="Remove grant" variant="danger" onClick={() => set({ grants: d.grants.filter((_, j) => j !== i) })} />
          </div>
        ))}
        {clusters.length === 0 ? <p className="text-sm text-muted">No clusters yet.</p> : <Button icon="plus" onClick={() => set({ grants: [...d.grants, { cluster: clusters[0].name, namespace: "", role: "view" }] })}>Add cluster access</Button>}
      </fieldset>
      {d.existing && <Checkbox label="Disabled (cannot sign in; Kubernetes access removed)" checked={d.disabled} onChange={(e) => set({ disabled: e.target.checked })} />}
    </Modal>
  );
}

export function UsersPage() {
  const { users, meta, can } = useSession();
  const toast = useToast();
  const [draft, setDraft] = useState<Draft | null>(null);
  const [del, setDel] = useState<User | null>(null);
  if (!can("users.manage")) return <Alert>You need the users.manage permission.</Alert>;
  return (
    <>
      <PageHeader title="Users & access" icon={<Icon name="users" width={20} height={20} className="text-muted" />}>
        <Button variant="primary" icon="plus" onClick={() => setDraft({ existing: false, username: "", password: "", permissions: [], grants: [], disabled: false })}>New user</Button>
      </PageHeader>
      <Card flush>
        <SimpleTable head={["User", "Console permissions", "Cluster access", "Created", ""]}>
          <tr>
            <td className="font-semibold">{meta.superUser} <Badge tone="warn" className="ml-1">super admin</Badge></td>
            <td className="text-muted">everything</td>
            <td className="text-muted">every cluster (cluster admin)</td>
            <td className="text-xs text-muted">from RUSTKUBEADMIN_USER</td>
            <td />
          </tr>
          {users.map((u) => (
            <tr key={u.username}>
              <td className="font-semibold">{u.username}{u.disabled && <Badge tone="danger" className="ml-2">disabled</Badge>}</td>
              <td>{u.permissions.length ? <span className="flex flex-wrap gap-1">{u.permissions.map((p) => <Badge key={p} tone="info">{p}</Badge>)}</span> : <span className="text-muted">none</span>}</td>
              <td className="text-xs">{u.grants.length ? u.grants.map((g, i) => <div key={i}><b>{g.cluster}</b>{g.namespace ? `/${g.namespace}` : " (all)"} · {ROLE_LABEL[g.role]}</div>) : <span className="text-muted">none</span>}</td>
              <td className="text-xs text-muted">{age(u.created)} ago by {u.createdBy}</td>
              <td className="whitespace-nowrap text-right">
                <span className="inline-flex gap-1">
                  <IconButton icon="edit" label="Edit user" onClick={() => setDraft({ existing: true, username: u.username, password: "", permissions: u.permissions, grants: u.grants, disabled: u.disabled })} />
                  <IconButton icon="trash" label="Delete user" variant="danger" onClick={() => setDel(u)} />
                </span>
              </td>
            </tr>
          ))}
        </SimpleTable>
        {users.length === 0 && <Empty>No users yet. Create one and give it console permissions and cluster access.</Empty>}
      </Card>
      <Card title="How access works">
        <ul className="list-disc space-y-1 pl-5 text-sm text-muted">
          <li><b className="text-fg">Console permissions</b> decide what someone may do in rustkubeadmin: manage users, machines, clusters, or read the activity log.</li>
          <li><b className="text-fg">Cluster access</b> decides what they may do inside a cluster. The console acts as <code>rustkubeadmin:&lt;user&gt;</code> (Kubernetes impersonation), so the cluster&apos;s RBAC enforces it — the same rules apply to the kubeconfig a user downloads.</li>
          <li>Roles: <b>View</b> reads everything except Secrets, <b>Edit</b> changes workloads and Secrets, <b>Admin</b> also manages RBAC (cluster-wide: cluster-admin).</li>
        </ul>
      </Card>
      {draft && <UserModal draft={draft} onClose={() => setDraft(null)} />}
      {del && <Confirm title="Delete user" confirmLabel="Delete" message={<>Delete <b>{del.username}</b>? Their sessions end and their Kubernetes bindings are removed.</>} onClose={() => setDel(null)}
        onConfirm={async () => { await api("/api/users/delete", { username: del.username }); toast("User deleted"); }} />}
    </>
  );
}
