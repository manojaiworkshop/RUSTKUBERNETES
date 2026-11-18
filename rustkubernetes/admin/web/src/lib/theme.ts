"use client";
import { useCallback, useEffect, useState } from "react";

export type Theme = "light" | "dark";

/** Runs before the first paint (see app/layout.tsx) so the page never flashes the wrong theme. */
export const THEME_SCRIPT = `try{var t=localStorage.getItem('theme')||(matchMedia('(prefers-color-scheme: dark)').matches?'dark':'light');document.documentElement.dataset.theme=t}catch(e){}`;

export function useTheme(): [Theme, () => void] {
  const [theme, set] = useState<Theme>("light");
  useEffect(() => set((document.documentElement.dataset.theme as Theme) || "light"), []);
  const toggle = useCallback(() => {
    const next: Theme = document.documentElement.dataset.theme === "dark" ? "light" : "dark";
    document.documentElement.dataset.theme = next;
    try { localStorage.setItem("theme", next); } catch { /* private mode */ }
    set(next);
  }, []);
  return [theme, toggle];
}
