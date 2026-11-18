"use client";
import { createContext, useCallback, useContext, useEffect, useState, type ReactNode } from "react";
import { api } from "./api";
import type { ClusterSummary, Job, Machine, Me, Meta, User } from "./types";
import { useEvents } from "./useEvents";

interface Ctx {
  me: Me;
  meta: Meta;
  clusters: ClusterSummary[];
  machines: Machine[];
  jobs: Job[];
  /** Console users (only for users with `users.manage`). */
  users: User[];
  /** True while the event stream is connected (live updates are flowing). */
  live: boolean;
  can: (perm: string) => boolean;
  reloadMeta: () => Promise<void>;
}

const SessionCtx = createContext<Ctx | null>(null);

export function useSession(): Ctx {
  const c = useContext(SessionCtx);
  if (!c) throw new Error("useSession outside SessionProvider");
  return c;
}

/** Loads who is signed in, then keeps clusters, machines, jobs and users current from the server's event stream. */
export function SessionProvider({ children, fallback }: { children: ReactNode; fallback: ReactNode }) {
  const [me, setMe] = useState<Me | null>(null);
  const [meta, setMeta] = useState<Meta | null>(null);
  const [clusters, setClusters] = useState<ClusterSummary[]>([]);
  const [machines, setMachines] = useState<Machine[]>([]);
  const [jobs, setJobs] = useState<Job[]>([]);
  const [users, setUsers] = useState<User[]>([]);
  const [live, setLive] = useState(false);

  const reloadMeta = useCallback(async () => setMeta(await api<Meta>("/api/meta")), []);
  useEffect(() => {
    api<{ user: Me }>("/api/session").then((s) => setMe(s.user)).catch(() => { /* api() already redirected to /login */ });
    reloadMeta().catch(() => {});
  }, [reloadMeta]);
  const can = useCallback((p: string) => !!me && (me.superadmin || me.permissions.includes(p)), [me]);

  const topics = ["clusters", "machines", "jobs", ...(me && (me.superadmin || me.permissions.includes("users.manage")) ? ["users"] : [])];
  useEvents(topics, {
    clusters: (c: ClusterSummary[]) => { setClusters(c); setLive(true); },
    machines: (m: Machine[]) => setMachines(m),
    jobs: (j: Job[]) => setJobs(j),
    users: (u: User[]) => setUsers(u),
    lost: () => setLive(false),
  }, !!me);

  if (!me || !meta) return <>{fallback}</>;
  return <SessionCtx.Provider value={{ me, meta, clusters, machines, jobs, users, live, can, reloadMeta }}>{children}</SessionCtx.Provider>;
}
