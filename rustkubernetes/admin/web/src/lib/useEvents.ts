"use client";
import { useEffect, useRef } from "react";

/**
 * Subscribes to the server's event stream (`/api/events`). Each topic is delivered once on connect and again whenever
 * its content changes on the server, so a page shows live data without polling. The browser reconnects by itself.
 */
export function useEvents(topics: string[], handlers: Record<string, (data: never) => void>, enabled = true) {
  const ref = useRef(handlers);
  ref.current = handlers;
  const key = topics.join(",");
  useEffect(() => {
    if (!enabled || !key) return;
    const es = new EventSource(`/api/events?topics=${encodeURIComponent(key)}`);
    const names = key.split(",");
    es.onerror = () => ref.current.lost?.(undefined as never);
    const listeners = names.map((t) => {
      const fn = (e: MessageEvent) => {
        try { ref.current[t]?.(JSON.parse(e.data) as never); } catch { /* ignore a malformed frame */ }
      };
      es.addEventListener(t, fn);
      return [t, fn] as const;
    });
    return () => {
      for (const [t, fn] of listeners) es.removeEventListener(t, fn);
      es.close();
    };
  }, [key, enabled]);
}
