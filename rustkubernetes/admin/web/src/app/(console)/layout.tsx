"use client";
import type { ReactNode } from "react";
import { AppShell } from "@/components/layout/AppShell";
import { Spinner } from "@/components/ui/Feedback";
import { SessionProvider } from "@/lib/session";

/** Everything under here needs a sign-in: the provider loads it (or sends the browser to /login). */
export default function ConsoleLayout({ children }: { children: ReactNode }) {
  return (
    <SessionProvider fallback={<div className="grid h-dvh place-items-center"><Spinner /></div>}>
      <AppShell>{children}</AppShell>
    </SessionProvider>
  );
}
