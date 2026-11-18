/** The REST client. Every call is same-origin and carries the session cookie; a 401 sends the browser to the sign-in page. */

export class ApiError extends Error {
  constructor(message: string, public status: number) {
    super(message);
  }
}

function toLogin() {
  if (typeof window !== "undefined" && !window.location.pathname.startsWith("/login")) window.location.replace("/login");
}

async function parse(r: Response, path: string): Promise<unknown> {
  const text = await r.text();
  let j: Record<string, unknown> | null = null;
  try { j = text ? JSON.parse(text) : {}; } catch { /* not JSON */ }
  if (r.status === 401 && path !== "/api/login") {
    toLogin();
    throw new ApiError("not signed in", 401);
  }
  if (!r.ok) {
    const msg = (j && (typeof j.error === "string" ? j.error : typeof j.message === "string" ? j.message : null)) || r.statusText || `HTTP ${r.status}`;
    throw new ApiError(String(msg), r.status);
  }
  return j ?? text;
}

/** GET when `body` is undefined, otherwise POST it as JSON. */
export async function api<T = unknown>(path: string, body?: unknown): Promise<T> {
  const init: RequestInit = body === undefined ? {} : { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify(body) };
  let r: Response;
  try {
    r = await fetch(path, init);
  } catch {
    throw new ApiError("rustkubeadmin is not reachable", 0);
  }
  return (await parse(r, path)) as T;
}

/** A Kubernetes API call on `cluster`, made as the signed-in user (RBAC applies). */
export async function kube<T = unknown>(cluster: string, path: string, method = "GET", body?: unknown, contentType = "application/json"): Promise<T> {
  const init: RequestInit = { method };
  if (body !== undefined) {
    init.headers = { "Content-Type": contentType };
    init.body = typeof body === "string" ? body : JSON.stringify(body);
  }
  const r = await fetch(`/api/kube/${encodeURIComponent(cluster)}${path}`, init);
  return (await parse(r, path)) as T;
}

/** Plain-text Kubernetes response (pod logs). */
export async function kubeText(cluster: string, path: string): Promise<string> {
  const r = await fetch(`/api/kube/${encodeURIComponent(cluster)}${path}`);
  if (!r.ok) {
    let msg = r.statusText;
    try { const j = await r.json(); msg = j.message || j.error || msg; } catch { /* keep */ }
    if (r.status === 401) toLogin();
    throw new ApiError(msg, r.status);
  }
  return r.text();
}

export function qs(params: Record<string, string | number | undefined | null | boolean>): string {
  const u = new URLSearchParams();
  for (const [k, v] of Object.entries(params)) if (v !== undefined && v !== null && v !== "" && v !== false) u.set(k, String(v));
  return u.toString();
}
