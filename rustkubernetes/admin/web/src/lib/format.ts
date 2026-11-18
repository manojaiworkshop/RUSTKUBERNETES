/** `5s`, `12m`, `3h`, `4d` since an RFC 3339 timestamp (like kubectl's AGE). */
export function age(ts?: string | null): string {
  if (!ts) return "–";
  const t = Date.parse(ts);
  if (Number.isNaN(t)) return "–";
  const s = Math.max(0, Math.floor((Date.now() - t) / 1000));
  if (s < 60) return `${s}s`;
  if (s < 3600) return `${Math.floor(s / 60)}m`;
  if (s < 86400) return `${Math.floor(s / 3600)}h`;
  return `${Math.floor(s / 86400)}d`;
}

/** Kubernetes memory quantity (`16384000Ki`) or kB count → `15.6 GiB`. */
export function memory(q?: string): string {
  if (!q) return "–";
  const m = /^(\d+)(Ki|Mi|Gi)?$/.exec(q);
  if (!m) return q;
  const mult = m[2] === "Gi" ? 1024 ** 3 : m[2] === "Mi" ? 1024 ** 2 : 1024;
  const gib = (Number(m[1]) * mult) / 1024 ** 3;
  return gib >= 1 ? `${gib.toFixed(1)} GiB` : `${(gib * 1024).toFixed(0)} MiB`;
}

export function dateTime(ts?: string | null): string {
  if (!ts) return "–";
  const d = new Date(ts);
  return Number.isNaN(d.getTime()) ? ts : d.toLocaleString();
}

export const ROLE_LABEL: Record<string, string> = { admin: "Admin", edit: "Edit", view: "View" };
