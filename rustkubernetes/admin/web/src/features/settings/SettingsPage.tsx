"use client";
import { useState, type FormEvent } from "react";
import { Button, IconButton } from "@/components/ui/Button";
import { Card, PageHeader } from "@/components/ui/Card";
import { Alert, Badge, useToast } from "@/components/ui/Feedback";
import { Field, Input } from "@/components/ui/Form";
import { Icon } from "@/components/ui/icons";
import { api } from "@/lib/api";
import { copyText } from "@/lib/clipboard";
import { dateTime } from "@/lib/format";
import { useSession } from "@/lib/session";

function ChangePassword() {
  const toast = useToast();
  const [cur, setCur] = useState("");
  const [next, setNext] = useState("");
  const [error, setError] = useState("");
  const submit = async (e: FormEvent) => {
    e.preventDefault();
    setError("");
    try {
      await api("/api/me/password", { current: cur, new: next });
      setCur("");
      setNext("");
      toast("Password changed");
    } catch (x) { setError((x as Error).message); }
  };
  return (
    <form onSubmit={submit} className="max-w-md space-y-3">
      {error && <Alert>{error}</Alert>}
      <Field label="Current password"><Input type="password" autoComplete="current-password" value={cur} onChange={(e) => setCur(e.target.value)} required /></Field>
      <Field label="New password" hint="at least 8 characters"><Input type="password" autoComplete="new-password" value={next} onChange={(e) => setNext(e.target.value)} required /></Field>
      <Button type="submit" variant="primary">Change password</Button>
    </form>
  );
}

export function SettingsPage() {
  const { me, meta, can, reloadMeta } = useSession();
  const toast = useToast();
  const b = meta.bundle;
  return (
    <>
      <PageHeader title="Settings" icon={<Icon name="gear" width={20} height={20} className="text-muted" />} />
      <Card title="Your account">
        <p className="mb-3">Signed in as <b>{me.username}</b> {me.superadmin && <Badge tone="warn">super admin</Badge>}</p>
        {me.superadmin ? <p className="text-muted">The super admin&apos;s credentials come from <code>RUSTKUBEADMIN_USER</code> and <code>RUSTKUBEADMIN_PASSWORD</code> in rustkubeadmin&apos;s environment; change them there and restart.</p> : <ChangePassword />}
      </Card>
      <Card title="rustkubeadmin SSH key">
        <p className="mb-2 text-sm text-muted">To add machines over SSH without passwords, put this key into <code>~/.ssh/authorized_keys</code> of the SSH user on each machine.</p>
        <div className="flex items-start gap-2">
          <code className="block flex-1 break-all rounded-md border border-line bg-subtle p-3 text-xs">{meta.sshPublicKey || "(ssh-keygen is not available)"}</code>
          <IconButton icon="copy" label="Copy key" onClick={async () => { await copyText(meta.sshPublicKey); toast("Copied"); }} />
        </div>
      </Card>
      <Card title="Offline bundle" actions={can("machines.manage") && <Button icon="sync" onClick={async () => { await api("/api/bundle/build", {}); toast("Building the bundle (see Activity)"); setTimeout(reloadMeta, 4000); }}>Build now</Button>}>
        {meta.missingBinaries.length > 0 && <Alert>Missing binaries in <code>{b.binaries_dir}</code>: {meta.missingBinaries.join(", ")}.</Alert>}
        <dl className="grid grid-cols-[10rem_minmax(0,1fr)] gap-y-1 text-sm">
          <dt className="text-muted">Status</dt><dd>{b.built ? <Badge tone="success">built</Badge> : <Badge>built on the first install</Badge>}</dd>
          <dt className="text-muted">Version</dt><dd className="font-mono">{b.version ?? "–"}</dd>
          <dt className="text-muted">Size</dt><dd>{b.size_human ?? "–"}</dd>
          <dt className="text-muted">Built</dt><dd>{dateTime(b.created)}</dd>
          <dt className="text-muted">Architecture</dt><dd>{b.arch ?? "–"}</dd>
          <dt className="text-muted">Binaries from</dt><dd className="truncate font-mono text-xs">{b.binaries_dir}</dd>
          <dt className="text-muted">Images</dt><dd className="font-mono text-xs">{(b.images ?? []).join(", ") || "–"}</dd>
        </dl>
        <p className="mt-3 text-xs text-muted">The bundle holds every binary, the node service manager, the installer and the container images (pause, CoreDNS, kube-proxy). It is rebuilt automatically when a binary or image changes.</p>
      </Card>
      <Card title="About">rustkubeadmin {meta.version} · console for rustkubernetes</Card>
    </>
  );
}
