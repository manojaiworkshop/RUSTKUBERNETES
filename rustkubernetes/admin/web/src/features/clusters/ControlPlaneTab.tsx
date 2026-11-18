"use client";
import { useState } from "react";
import { Button } from "@/components/ui/Button";
import { Card } from "@/components/ui/Card";
import { Alert, Badge, Loading } from "@/components/ui/Feedback";
import { Modal } from "@/components/ui/Modal";
import { StatusBadge } from "@/components/ui/Status";
import { api, qs } from "@/lib/api";
import { useAsync } from "@/lib/useAsync";

interface MachineServices { machine: string; machineId: string; node: string; master: boolean; services: { name: string; state: string }[]; error: string | null }

/** Services of a managed cluster on each machine, with their logs. */
export function ControlPlaneTab({ cluster }: { cluster: string }) {
  const { data, error, loading, reload } = useAsync(() => api<MachineServices[]>(`/api/clusters/services?${qs({ name: cluster })}`), [cluster]);
  const [log, setLog] = useState<{ machine: MachineServices; service: string; text: string } | null>(null);
  const open = async (m: MachineServices, service: string) => {
    const r = await api<{ log: string }>(`/api/clusters/service-log?${qs({ name: cluster, machine: m.machineId, service, lines: 400 })}`);
    setLog({ machine: m, service, text: r.log });
  };
  return (
    <>
      <div className="mb-3 flex items-center gap-2">
        <p className="mr-auto text-sm text-muted">Each machine runs the cluster&apos;s services under systemd (or a fallback supervisor); logs are kept per service.</p>
        <Button icon="sync" onClick={reload}>Refresh</Button>
      </div>
      <Loading error={error} loading={loading}>
        {data && <div className="grid gap-4 xl:grid-cols-2">
          {data.map((m) => (
            <Card key={m.machineId} title={<>{m.machine} <Badge tone={m.master ? "warn" : "info"}>{m.master ? "master" : "worker"}</Badge></>} flush>
              {m.error && <Alert className="m-3">{m.error}</Alert>}
              {m.services.map((s) => (
                <div key={s.name} className="flex items-center gap-2 border-t border-line px-4 py-2 first:border-t-0">
                  <span className="mr-auto font-mono text-sm">{s.name}</span>
                  <StatusBadge status={s.state === "active" ? "running" : s.state === "activating" ? "provisioning" : "failed"} label={s.state} />
                  <Button icon="logs" onClick={() => open(m, s.name)}>Logs</Button>
                </div>
              ))}
            </Card>
          ))}
        </div>}
      </Loading>
      {log && (
        <Modal title={`${log.service} on ${log.machine.machine}`} wide onClose={() => setLog(null)}>
          <pre className="m-0 h-[60dvh] overflow-auto rounded-md border border-line bg-[#0d1117] p-3 font-mono text-[11px] leading-relaxed text-[#e6edf3]">{log.text.replace(/\x1b\[[0-9;]*m/g, "") || "(empty)"}</pre>
        </Modal>
      )}
    </>
  );
}
