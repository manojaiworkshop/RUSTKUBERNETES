import { cn } from "@/lib/cn";
import { Badge } from "./Feedback";

type Tone = "neutral" | "success" | "warn" | "info" | "danger";

const STATUS_TONE: Record<string, Tone> = {
  running: "success", ready: "success", succeeded: "success", Running: "success", Succeeded: "success", Completed: "neutral", Active: "success",
  provisioning: "info", installing: "info", new: "neutral", stopped: "neutral", deleting: "warn",
  Pending: "warn", ContainerCreating: "warn", PodInitializing: "warn", Terminating: "warn", Terminating_: "warn",
  failed: "danger", Failed: "danger", Error: "danger", CrashLoopBackOff: "danger", ImagePullBackOff: "danger", ErrImagePull: "danger", OOMKilled: "danger", Unknown: "warn",
};

export function toneOf(status: string): Tone {
  if (status.startsWith("Init:")) return "warn";
  return STATUS_TONE[status] ?? "neutral";
}

/** A status pill with a coloured dot (cluster, machine, pod and job states). */
export function StatusBadge({ status, label }: { status: string; label?: string }) {
  const tone = toneOf(status);
  const dot = { success: "bg-success", warn: "bg-warn", info: "bg-link", danger: "bg-danger", neutral: "bg-muted" }[tone];
  const pulse = ["provisioning", "installing", "running_job", "ContainerCreating", "Pending", "deleting", "Terminating"].includes(status);
  return (
    <Badge tone={tone} className="inline-flex items-center gap-1.5">
      <span className={cn("size-1.5 rounded-full", dot, pulse && "animate-pulse")} />
      {label ?? status}
    </Badge>
  );
}

export function Dot({ tone }: { tone: Tone }) {
  return <span className={cn("inline-block size-2 shrink-0 rounded-full", { success: "bg-success", warn: "bg-warn", info: "bg-link", danger: "bg-danger", neutral: "bg-line" }[tone])} />;
}

/** A segmented bar (e.g. pods running / pending / failed). */
export function SegmentBar({ parts, total }: { parts: { value: number; className: string; label: string }[]; total: number }) {
  return (
    <div className="flex h-2 w-full overflow-hidden rounded-full bg-inset" title={parts.map((p) => `${p.value} ${p.label}`).join(" · ")}>
      {total > 0 && parts.map((p) => p.value > 0 && <div key={p.label} className={cn("h-full transition-[width] duration-500", p.className)} style={{ width: `${(p.value / total) * 100}%` }} />)}
    </div>
  );
}
