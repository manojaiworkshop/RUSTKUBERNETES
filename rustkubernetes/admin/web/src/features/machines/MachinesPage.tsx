"use client";
import { useSearchParams } from "next/navigation";
import { useEffect, useState } from "react";
import { Button, IconButton } from "@/components/ui/Button";
import { Card, PageHeader } from "@/components/ui/Card";
import { Alert, Badge, Empty, useToast } from "@/components/ui/Feedback";
import { Field, Input, Select, Textarea } from "@/components/ui/Form";
import { Icon } from "@/components/ui/icons";
import { JobLog } from "@/components/ui/JobLog";
import { Confirm, Modal } from "@/components/ui/Modal";
import { StatusBadge } from "@/components/ui/Status";
import { Tabs } from "@/components/ui/Tabs";
import { api } from "@/lib/api";
import { copyText } from "@/lib/clipboard";
import { cn } from "@/lib/cn";
import { age, memory } from "@/lib/format";
import { useSession } from "@/lib/session";
import type { Machine } from "@/lib/types";

interface Draft { transport: "local" | "ssh"; name: string; address: string; sshUser: string; sshPort: number; sshAuth: "admin-key" | "key" | "password"; privateKey: string; password: string; prefix: string }

function AddMachine({ onClose }: { onClose: () => void }) {
  const { meta, machines } = useSession();
  const toast = useToast();
  const local = !meta.hasLocalMachine && !machines.some((m) => m.transport === "local");
  const [d, setD] = useState<Draft>({ transport: local ? "local" : "ssh", name: local ? "this-machine" : "", address: local ? meta.localAddress : "", sshUser: "", sshPort: 22, sshAuth: "admin-key", privateKey: "", password: "", prefix: "$HOME/.local/rustkube" });
  const set = (p: Partial<Draft>) => setD({ ...d, ...p });
  return (
    <Modal title="Add a machine" confirmLabel="Add and install" wide onClose={onClose} onConfirm={async () => {
      await api("/api/machines", { ...d, name: d.name.trim(), address: d.address.trim() });
      toast(`Installing rustkubernetes on ${d.name}`);
    }}>
      <Tabs tabs={[{ id: "local", label: "This machine" }, { id: "ssh", label: "Another machine (SSH)" }]} value={d.transport}
        onChange={(t) => set(t === "local" ? { transport: "local", name: d.name || "this-machine", address: meta.localAddress } : { transport: "ssh", address: "" })} className="!mb-2" />
      {d.transport === "local" && !local && <Alert>This machine is already added.</Alert>}
      <div className="grid grid-cols-2 gap-3">
        <Field label="Name" hint="becomes the node name"><Input value={d.name} onChange={(e) => set({ name: e.target.value })} required autoFocus /></Field>
        <Field label="IP address" hint="other machines and pods use it"><Input value={d.address} onChange={(e) => set({ address: e.target.value })} placeholder="192.168.1.20" required /></Field>
      </div>
      {d.transport === "ssh" && (
        <>
          <div className="grid grid-cols-[minmax(0,1fr)_7rem] gap-3">
            <Field label="SSH user" hint="in the docker group; root is not required"><Input value={d.sshUser} onChange={(e) => set({ sshUser: e.target.value })} required /></Field>
            <Field label="Port"><Input type="number" value={d.sshPort} onChange={(e) => set({ sshPort: Number(e.target.value) })} /></Field>
          </div>
          <Field label="Authentication">
            <Select value={d.sshAuth} onChange={(e) => set({ sshAuth: e.target.value as Draft["sshAuth"] })}>
              <option value="admin-key">rustkubeadmin&apos;s SSH key (recommended)</option>
              <option value="key">A private key I paste</option>
              <option value="password">Password</option>
            </Select>
          </Field>
          {d.sshAuth === "admin-key" && (
            <div className="rounded-md border border-line bg-canvas p-3 text-sm">
              <p className="mb-2">Add this public key to <code>~/.ssh/authorized_keys</code> of <b>{d.sshUser || "the user"}</b> on the machine:</p>
              <div className="flex items-start gap-2">
                <code className="block flex-1 break-all rounded bg-inset p-2 text-xs">{meta.sshPublicKey}</code>
                <IconButton icon="copy" label="Copy key" onClick={async () => { await copyText(meta.sshPublicKey); toast("Copied"); }} />
              </div>
              <p className="mt-2 text-xs text-muted">e.g. <code>echo &apos;{meta.sshPublicKey.slice(0, 24)}…&apos; &gt;&gt; ~/.ssh/authorized_keys</code></p>
            </div>
          )}
          {d.sshAuth === "key" && <Field label="Private key" hint="stored on the rustkubeadmin server, readable only by it"><Textarea rows={6} className="font-mono text-xs" value={d.privateKey} onChange={(e) => set({ privateKey: e.target.value })} placeholder="-----BEGIN OPENSSH PRIVATE KEY-----" /></Field>}
          {d.sshAuth === "password" && <Field label="Password" hint="used through sshpass; a key is safer"><Input type="password" autoComplete="new-password" value={d.password} onChange={(e) => set({ password: e.target.value })} /></Field>}
        </>
      )}
      <Field label="Install directory" hint="absolute, or under $HOME (no root needed)"><Input value={d.prefix} onChange={(e) => set({ prefix: e.target.value })} /></Field>
      <div className="rounded-md bg-subtle p-3 text-xs text-muted">
        <b className="text-fg">Requirements:</b> 64-bit Linux, Docker with the user in the <code>docker</code> group. The offline bundle ({meta.bundle.size_human ?? "built on first use"}) carries every binary and container image — the machine needs no internet access.
      </div>
    </Modal>
  );
}

function MachineCard({ m, open, onToggle }: { m: Machine; open: boolean; onToggle: () => void }) {
  const { jobs, can } = useSession();
  const toast = useToast();
  const [del, setDel] = useState(false);
  const job = jobs.find((j) => j.target === m.id);
  const f = m.facts;
  return (
    <div id={`m-${m.id}`} className={cn("overflow-hidden rounded-md border bg-canvas", open ? "border-link" : "border-line")}>
      <div className="flex cursor-pointer flex-wrap items-center gap-3 p-4 hover:bg-subtle" onClick={onToggle}>
        <span className="grid size-9 place-items-center rounded-md bg-inset text-muted"><Icon name="server" /></span>
        <div className="min-w-0 flex-1">
          <b className="block truncate">{m.name}</b>
          <span className="font-mono text-xs text-muted">{m.transport === "ssh" ? `${m.sshUser}@${m.address}:${m.sshPort}` : `${m.address} · this machine`}</span>
        </div>
        {m.cluster && <Badge tone={m.role === "master" ? "warn" : "info"}>{m.role} of {m.cluster}</Badge>}
        {f.cpus && <span className="hidden text-xs text-muted md:inline">{f.cpus} CPU · {memory(`${f.memory_kb}Ki`)} · Docker {f.docker}</span>}
        <StatusBadge status={m.status} />
        <Icon name="chevron" className={cn("text-muted transition-transform", open && "rotate-90")} />
      </div>
      {open && (
        <div className="space-y-4 border-t border-line bg-subtle p-4">
          {m.status === "failed" && m.message && <Alert className="!mb-0">{m.message}</Alert>}
          {Object.keys(f).length > 0 && (
            <dl className="grid grid-cols-2 gap-x-6 gap-y-1 text-sm sm:grid-cols-[8rem_minmax(0,1fr)_8rem_minmax(0,1fr)]">
              {[["Hostname", f.hostname], ["OS", f.os], ["Kernel", f.kernel], ["Arch", f.arch], ["CPUs", f.cpus], ["Memory", memory(`${f.memory_kb}Ki`)], ["Docker", f.docker], ["Supervisor", f.supervisor], ["Install dir", f.prefix], ["Version", m.version], ["br_netfilter", f.br_netfilter === "1" ? "on" : "off"], ["Added", `${age(m.created)} ago by ${m.createdBy}`]].map(([k, v]) => (
                <div key={k} className="contents"><dt className="text-muted">{k}</dt><dd className="truncate" title={v}>{v || "–"}</dd></div>
              ))}
            </dl>
          )}
          {job && <JobLog job={job} />}
          {can("machines.manage") && (
            <div className="flex flex-wrap gap-2">
              <Button icon="zap" onClick={async () => {
                const r = await api<{ ok: boolean; output: string }>("/api/machines/test", { id: m.id });
                toast(r.ok ? `Reachable: ${r.output}` : r.output, r.ok ? "ok" : "error");
              }}>Test connection</Button>
              <Button icon="download" disabled={m.status === "installing"} onClick={async () => { await api("/api/machines/install", { id: m.id }); toast("Installer started"); }}>{m.status === "ready" ? "Reinstall / upgrade" : "Install"}</Button>
              <Button variant="danger" icon="trash" disabled={!!m.cluster} onClick={() => setDel(true)}>Remove</Button>
            </div>
          )}
        </div>
      )}
      {del && <Confirm title="Remove machine" confirmLabel="Remove" message={<>Remove <b>{m.name}</b> from the console and delete the installed binaries from it?</>} onClose={() => setDel(false)}
        onConfirm={async () => { await api("/api/machines/delete", { id: m.id }); toast("Machine removed"); }} />}
    </div>
  );
}

export function MachinesPage() {
  const { machines, meta, can } = useSession();
  const sp = useSearchParams();
  const [adding, setAdding] = useState(false);
  const [open, setOpen] = useState<string | null>(sp.get("id"));
  useEffect(() => { const id = sp.get("id"); if (id) { setOpen(id); document.getElementById(`m-${id}`)?.scrollIntoView({ block: "center" }); } }, [sp]);
  return (
    <>
      <PageHeader title="Machines" icon={<Icon name="server" width={20} height={20} className="text-muted" />}>
        {can("machines.manage") && <Button variant="primary" icon="plus" onClick={() => setAdding(true)}>Add machine</Button>}
      </PageHeader>
      {meta.missingBinaries.length > 0 && <Alert>The offline bundle cannot be built: {meta.missingBinaries.join(", ")} missing in <code>{meta.bundle.binaries_dir}</code>. Build them with <code>cargo build --release</code> or start rustkubeadmin with <code>--binaries</code>.</Alert>}
      {machines.length === 0 ? (
        <Card><Empty>No machines yet. Add this machine or any Linux server you can reach over SSH; the installer copies everything from the offline bundle.</Empty></Card>
      ) : (
        <div className="space-y-3">{machines.map((m) => <MachineCard key={m.id} m={m} open={open === m.id} onToggle={() => setOpen(open === m.id ? null : m.id)} />)}</div>
      )}
      {adding && <AddMachine onClose={() => setAdding(false)} />}
    </>
  );
}
