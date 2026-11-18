"use client";
import { useRouter } from "next/navigation";
import { useEffect } from "react";
import { Spinner } from "@/components/ui/Feedback";

/** `/` only decides where to go: the console when signed in, the sign-in page otherwise. */
export default function Home() {
  const router = useRouter();
  useEffect(() => {
    fetch("/api/session").then((r) => router.replace(r.ok ? "/overview" : "/login")).catch(() => router.replace("/login"));
  }, [router]);
  return <div className="grid h-dvh place-items-center"><Spinner /></div>;
}
