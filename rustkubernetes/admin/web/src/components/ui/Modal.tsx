"use client";
import { useEffect, useState, type ReactNode } from "react";
import { Button } from "./Button";
import { Alert } from "./Feedback";

/**
 * A dialog with a title, a body and Cancel / confirm buttons. `onConfirm` may be async: while it runs the button is
 * disabled, and an error it throws is shown inside the dialog, which then stays open.
 */
export function Modal({ title, children, confirmLabel, danger, onConfirm, onClose, wide }: {
  title: ReactNode; children: ReactNode; confirmLabel?: string; danger?: boolean; wide?: boolean;
  onConfirm?: () => void | Promise<void>; onClose: () => void;
}) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  useEffect(() => {
    const key = (e: KeyboardEvent) => { if (e.key === "Escape") onClose(); };
    window.addEventListener("keydown", key);
    return () => window.removeEventListener("keydown", key);
  }, [onClose]);
  const confirm = async () => {
    if (!onConfirm) return;
    setBusy(true);
    setError("");
    try {
      await onConfirm();
      onClose();
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setBusy(false);
    }
  };
  return (
    <div className="fixed inset-0 z-40 grid items-end justify-items-center overflow-auto bg-overlay sm:items-start sm:p-4 sm:pt-[8vh]" onMouseDown={(e) => { if (e.target === e.currentTarget) onClose(); }}>
      <form role="dialog" aria-modal="true" onSubmit={(e) => { e.preventDefault(); confirm(); }}
        className={`flex max-h-[92dvh] w-full flex-col rounded-t-xl border border-line bg-subtle shadow-2xl sm:rounded-xl ${wide ? "sm:max-w-3xl" : "sm:max-w-xl"}`}>
        <h3 className="border-b border-line px-4 py-3 text-sm font-semibold">{title}</h3>
        <div className="flex-1 space-y-3 overflow-auto p-4">
          {children}
          {error && <Alert className="!mb-0">{error}</Alert>}
        </div>
        <div className="flex justify-end gap-2 border-t border-line px-4 py-3">
          <Button onClick={onClose}>{onConfirm ? "Cancel" : "Close"}</Button>
          {onConfirm && <Button type="submit" variant={danger ? "danger" : "primary"} disabled={busy}>{busy ? "Working…" : confirmLabel ?? "OK"}</Button>}
        </div>
      </form>
    </div>
  );
}

/** "Are you sure?" for destructive actions. */
export function Confirm({ title, message, confirmLabel, onConfirm, onClose }: { title: string; message: ReactNode; confirmLabel: string; onConfirm: () => void | Promise<void>; onClose: () => void }) {
  return <Modal title={title} confirmLabel={confirmLabel} danger onConfirm={onConfirm} onClose={onClose}><p>{message}</p></Modal>;
}
