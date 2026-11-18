"use client";
import { useRouter } from "next/navigation";
import { useEffect, useState, type FormEvent } from "react";
import { Button, IconButton } from "@/components/ui/Button";
import { Alert } from "@/components/ui/Feedback";
import { Field, Input } from "@/components/ui/Form";
import { Icon, type IconName } from "@/components/ui/icons";
import { api } from "@/lib/api";
import { useTheme } from "@/lib/theme";

const POINTS: { icon: IconName; title: string; text: string }[] = [
  { icon: "cluster", title: "Clusters in minutes", text: "Pick a master and workers; certificates, services and DNS are set up for you." },
  { icon: "download", title: "Offline installer", text: "One bundle with every binary and image. Machines never need the internet." },
  { icon: "shield", title: "Users and RBAC", text: "Give people console permissions and namespace-level Kubernetes roles." },
  { icon: "zap", title: "Live", text: "Nodes, pods and installs update as they happen, without refreshing." },
];

/** Sign-in: a brand panel beside the form on wide screens, stacked on a phone. Both halves follow the light or dark theme. */
export function LoginScreen() {
  const router = useRouter();
  const [username, setUsername] = useState("");
  const [password, setPassword] = useState("");
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  const [theme, toggleTheme] = useTheme();

  useEffect(() => {
    try { setUsername(localStorage.getItem("login-user") || ""); } catch { /* nothing saved */ }
  }, []);

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    setBusy(true);
    setError("");
    try {
      await api("/api/login", { username, password });
      try { localStorage.setItem("login-user", username); } catch { /* private mode */ }
      router.replace("/overview");
    } catch (x) {
      setError((x as Error).message);
      setBusy(false);
    }
  };

  return (
    <main className="grid min-h-dvh lg:grid-cols-[minmax(0,5fr)_minmax(0,4fr)]">
      <section className="relative flex flex-col justify-between gap-8 overflow-hidden border-b border-line bg-subtle px-6 py-8 text-fg sm:px-10 lg:border-b-0 lg:border-r lg:px-14 lg:py-12">
        <div aria-hidden="true" className="pointer-events-none absolute -left-32 -top-32 size-[28rem] rounded-full bg-[#a371f7]/20 blur-3xl dark:bg-[#a371f7]/25" />
        <div aria-hidden="true" className="pointer-events-none absolute -bottom-40 right-0 size-[30rem] rounded-full bg-[#f78166]/25 blur-3xl dark:bg-[#f78166]/20" />
        <div className="relative flex items-center gap-3">
          <span className="grid size-10 place-items-center rounded-full bg-gradient-to-br from-[#f78166] to-[#a371f7] text-lg font-bold text-white">K</span>
          <span className="text-lg font-semibold">rustkubeadmin</span>
        </div>
        <div className="relative max-w-xl">
          <h1 className="text-3xl font-semibold leading-tight tracking-tight sm:text-4xl">Your Kubernetes clusters, <span className="bg-gradient-to-r from-[#f78166] to-[#a371f7] bg-clip-text text-transparent">in one console.</span></h1>
          <ul className="mt-8 hidden gap-5 sm:grid sm:grid-cols-2">
            {POINTS.map((p) => (
              <li key={p.title} className="flex gap-3">
                <span className="mt-0.5 grid size-8 shrink-0 place-items-center rounded-md border border-line bg-canvas text-accent"><Icon name={p.icon} /></span>
                <span><b className="block text-sm">{p.title}</b><span className="text-sm text-muted">{p.text}</span></span>
              </li>
            ))}
          </ul>
        </div>
        <p className="relative hidden text-xs text-muted lg:block">Manages rustkubernetes machines and clusters. Nothing is stored in the browser except your username.</p>
      </section>

      <section className="relative flex items-center justify-center px-4 py-8 sm:px-8">
        <IconButton icon={theme === "dark" ? "sun" : "moon"} label="Switch theme" className="absolute right-4 top-4" onClick={toggleTheme} />
        <form onSubmit={submit} className="w-full max-w-sm">
          <h2 className="mb-1 text-2xl font-semibold">Sign in</h2>
          <p className="mb-6 text-muted">Use your rustkubeadmin account.</p>
          {error && <Alert>{error}</Alert>}
          <div className="space-y-4 rounded-md border border-line bg-subtle p-4">
            <Field label="Username"><Input value={username} onChange={(e) => setUsername(e.target.value)} autoComplete="username" autoFocus required /></Field>
            <Field label="Password"><Input type="password" value={password} onChange={(e) => setPassword(e.target.value)} autoComplete="current-password" required /></Field>
            <Button type="submit" variant="primary" block lg disabled={busy}>{busy ? "Signing in…" : "Sign in"}</Button>
          </div>
          <p className="mt-4 text-xs text-muted">The super admin signs in with <code>RUSTKUBEADMIN_USER</code> and <code>RUSTKUBEADMIN_PASSWORD</code> from the server&apos;s environment; other accounts are created by an admin in the console.</p>
        </form>
      </section>
    </main>
  );
}
