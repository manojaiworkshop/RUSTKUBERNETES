"use client";
import { useEffect, useRef } from "react";
import { cn } from "@/lib/cn";
import type { Job } from "@/lib/types";
import { StatusBadge } from "./Status";

/** A job's log as a terminal pane that follows new lines while the job runs. */
export function JobLog({ job, className, compact }: { job: Job; className?: string; compact?: boolean }) {
  const ref = useRef<HTMLPreElement>(null);
  useEffect(() => {
    const el = ref.current;
    if (el) el.scrollTop = el.scrollHeight;
  }, [job.log.length]);
  return (
    <div className={cn("overflow-hidden rounded-md border border-line", className)}>
      {!compact && (
        <div className="flex items-center gap-2 border-b border-line bg-subtle px-3 py-1.5 text-xs">
          <b className="mr-auto truncate">{job.title}</b>
          <StatusBadge status={job.state === "running" ? "running_job" : job.state} label={job.state} />
        </div>
      )}
      <pre ref={ref} className="m-0 max-h-80 overflow-auto bg-[#0d1117] p-3 font-mono text-[12px] leading-relaxed text-[#e6edf3]">
        {job.log.map((l, i) => (
          <div key={i} className={cn(l.startsWith("✘") && "text-[#f85149]", l.startsWith("✔") && "text-[#3fb950]", l.startsWith("→") && "text-[#79c0ff]", l.startsWith("  ") && "text-[#8b949e]")}>{l}</div>
        ))}
        {job.state === "running" && <div className="animate-pulse text-[#8b949e]">▍</div>}
      </pre>
    </div>
  );
}
