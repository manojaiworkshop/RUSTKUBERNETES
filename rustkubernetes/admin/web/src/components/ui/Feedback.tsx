"use client";
import { createContext, useCallback, useContext, useState, type ReactNode } from "react";
import { cn } from "@/lib/cn";

type Tone = "neutral" | "success" | "warn" | "info" | "danger";
const tones: Record<Tone, string> = {
  neutral: "text-muted border-line",
  success: "text-success border-success/40",
  warn: "text-warn border-warn/40",
  info: "text-link border-link/40",
  danger: "text-danger border-danger/40",
};

/** A small rounded label. */
export function Badge({ tone = "neutral", children, className }: { tone?: Tone; children: ReactNode; className?: string }) {
  return <span className={cn("inline-block whitespace-nowrap rounded-full border px-2 text-xs font-medium leading-5", tones[tone], className)}>{children}</span>;
}

export function Alert({ tone = "danger", children, className }: { tone?: "danger" | "success" | "info"; children: ReactNode; className?: string }) {
  const c = tone === "danger" ? "border-danger/40 bg-danger/10 text-danger" : tone === "success" ? "border-success/40 bg-success/10 text-success" : "border-link/40 bg-link/10 text-fg";
  return <div role={tone === "danger" ? "alert" : "status"} className={cn("mb-4 rounded-md border px-4 py-3 [overflow-wrap:anywhere]", c, className)}>{children}</div>;
}

export function Spinner({ className }: { className?: string }) {
  return <span className={cn("inline-block size-4 animate-spin rounded-full border-2 border-line border-t-link align-[-3px]", className)} aria-label="Loading" />;
}

export function Empty({ children }: { children: ReactNode }) {
  return <div className="px-4 py-8 text-center text-muted">{children}</div>;
}

/** What a page shows while its data loads or when loading failed. */
export function Loading({ error, loading, children }: { error?: string; loading: boolean; children: ReactNode }) {
  if (error) return <Alert>{error}</Alert>;
  if (loading && !children) return <div className="py-10 text-center"><Spinner /></div>;
  return <>{children}</>;
}

export function Progress({ value, tone = "info" }: { value: number; tone?: "info" | "success" | "danger" }) {
  const c = tone === "success" ? "bg-success" : tone === "danger" ? "bg-danger" : "bg-link";
  return (
    <div className="h-2 w-full overflow-hidden rounded-full bg-inset" role="progressbar" aria-valuenow={Math.round(value)} aria-valuemin={0} aria-valuemax={100}>
      <div className={cn("h-full rounded-full transition-[width] duration-500", c)} style={{ width: `${Math.min(100, Math.max(0, value))}%` }} />
    </div>
  );
}

export function CodeBlock({ children }: { children: ReactNode }) {
  return <pre className="m-0 overflow-auto rounded-md border border-line bg-subtle p-4 font-mono text-xs leading-relaxed">{children}</pre>;
}

/** A line chart of the last values, drawn as one SVG path. */
export function Sparkline({ values, color = "var(--link)" }: { values: number[]; color?: string }) {
  const w = 300, h = 56, max = Math.max(1, ...values);
  const pts = values.map((v, i) => `${(i / Math.max(1, values.length - 1)) * w},${h - 4 - (v / max) * (h - 8)}`).join(" ");
  return (
    <svg viewBox={`0 0 ${w} ${h}`} width="100%" height={h} preserveAspectRatio="none" aria-hidden="true">
      <polyline fill="none" stroke={color} strokeWidth="2" vectorEffect="non-scaling-stroke" points={pts} />
    </svg>
  );
}

// ---- toasts ----

const ToastCtx = createContext<(msg: string, tone?: "ok" | "error") => void>(() => {});
export const useToast = () => useContext(ToastCtx);

export function ToastProvider({ children }: { children: ReactNode }) {
  const [items, set] = useState<{ id: number; msg: string; tone: "ok" | "error" }[]>([]);
  const push = useCallback((msg: string, tone: "ok" | "error" = "ok") => {
    const id = Date.now() + Math.random();
    set((x) => [...x, { id, msg, tone }]);
    setTimeout(() => set((x) => x.filter((t) => t.id !== id)), tone === "error" ? 6000 : 3000);
  }, []);
  return (
    <ToastCtx.Provider value={push}>
      {children}
      <div className="pointer-events-none fixed inset-x-4 bottom-4 z-50 flex flex-col items-end gap-2 sm:left-auto sm:right-6 sm:bottom-6" aria-live="polite">
        {items.map((t) => (
          <div key={t.id} className={cn("pointer-events-auto max-w-md rounded-md border bg-subtle px-4 py-3 shadow-lg [overflow-wrap:anywhere]", t.tone === "error" ? "border-danger/50 text-danger" : "border-line")}>{t.msg}</div>
        ))}
      </div>
    </ToastCtx.Provider>
  );
}
