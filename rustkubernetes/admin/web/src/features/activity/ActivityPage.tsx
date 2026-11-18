"use client";
import { useSearchParams } from "next/navigation";
import { useState } from "react";
import { Button } from "@/components/ui/Button";
import { Card, PageHeader } from "@/components/ui/Card";
import { Badge, Empty, Loading } from "@/components/ui/Feedback";
import { Icon } from "@/components/ui/icons";
import { JobLog } from "@/components/ui/JobLog";
import { SimpleTable } from "@/components/ui/SimpleTable";
import { StatusBadge } from "@/components/ui/Status";
import { Tabs } from "@/components/ui/Tabs";
import { api } from "@/lib/api";
import { cn } from "@/lib/cn";
import { age, dateTime } from "@/lib/format";
import { useSession } from "@/lib/session";
import type { AuditEntry } from "@/lib/types";
import { useAsync } from "@/lib/useAsync";

export function ActivityPage() {
  const { jobs } = useSession();
  const sp = useSearchParams();
  const [tab, setTab] = useState<"jobs" | "audit">("jobs");
  // undefined: nothing chosen yet → show the requested (or newest) job; null: everything collapsed
  const [chosen, setOpen] = useState<string | null | undefined>(undefined);
  const open = chosen === undefined ? (sp.get("job") ?? jobs[0]?.id ?? null) : chosen;
  const audit = useAsync(() => (tab === "audit" ? api<AuditEntry[]>("/api/audit?limit=500") : Promise.resolve([] as AuditEntry[])), [tab]);
  return (
    <>
      <PageHeader title="Activity" icon={<Icon name="activity" width={20} height={20} className="text-muted" />} />
      <Tabs tabs={[{ id: "jobs", label: <>Jobs <span className="rounded-full bg-inset px-1.5 text-xs">{jobs.length}</span></> }, { id: "audit", label: "Audit log" }]} value={tab} onChange={setTab} />
      {tab === "jobs" && (
        <div className="space-y-3">
          {jobs.length === 0 && <Card><Empty>No jobs yet. Installs and cluster operations appear here with their full logs.</Empty></Card>}
          {jobs.map((j) => (
            <div key={j.id} className={cn("overflow-hidden rounded-md border bg-canvas", open === j.id ? "border-link" : "border-line")}>
              <div className="flex cursor-pointer items-center gap-3 px-4 py-3 hover:bg-subtle" onClick={() => setOpen(open === j.id ? null : j.id)}>
                <Icon name={j.kind === "install" ? "download" : j.kind === "bundle" ? "file" : "cluster"} className="text-muted" />
                <b className="min-w-0 flex-1 truncate">{j.title}</b>
                <span className="hidden text-xs text-muted sm:inline">{j.user} · {age(j.started)} ago</span>
                <StatusBadge status={j.state === "running" ? "running_job" : j.state} label={j.state} />
                <Icon name="chevron" className={cn("text-muted transition-transform", open === j.id && "rotate-90")} />
              </div>
              {open === j.id && <div className="border-t border-line p-3"><JobLog job={j} compact /></div>}
            </div>
          ))}
        </div>
      )}
      {tab === "audit" && (
        <Card flush actions={<Button icon="sync" onClick={audit.reload}>Refresh</Button>} title="Who did what">
          <Loading error={audit.error} loading={audit.loading}>
            {audit.data && (audit.data.length === 0 ? <Empty>Nothing recorded yet.</Empty> : (
              <SimpleTable head={["Time", "User", "Action", "Detail"]}>
                {audit.data.map((e, i) => (
                  <tr key={i}>
                    <td className="whitespace-nowrap text-xs text-muted">{dateTime(e.time)}</td>
                    <td className="font-semibold">{e.user}</td>
                    <td><Badge tone={e.action.includes("failed") || e.action.includes("delete") ? "danger" : e.action.startsWith("kube.") ? "info" : "neutral"}>{e.action}</Badge></td>
                    <td className="max-w-[40rem] truncate text-xs" title={e.detail}>{e.detail}</td>
                  </tr>
                ))}
              </SimpleTable>
            ))}
          </Loading>
        </Card>
      )}
    </>
  );
}
