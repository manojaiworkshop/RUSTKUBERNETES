"use client";
import { useCallback, useEffect, useRef, useState } from "react";

/** Loads data for a page: `{data, error, loading, reload}`. A newer load supersedes an older one that is still running. */
export function useAsync<T>(load: () => Promise<T>, deps: unknown[]) {
  const [state, set] = useState<{ data?: T; error?: string; loading: boolean }>({ loading: true });
  const gen = useRef(0);
  // eslint-disable-next-line react-hooks/exhaustive-deps
  const fn = useCallback(load, deps);
  const reload = useCallback(async () => {
    const mine = ++gen.current;
    set((s) => ({ ...s, loading: true, error: undefined }));
    try {
      const data = await fn();
      if (mine === gen.current) set({ data, loading: false });
    } catch (e) {
      if (mine === gen.current) set({ error: (e as Error).message, loading: false });
    }
  }, [fn]);
  useEffect(() => { reload(); }, [reload]);
  return { ...state, reload, setData: (data: T) => set({ data, loading: false }) };
}
