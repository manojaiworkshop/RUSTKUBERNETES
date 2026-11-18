import type { InputHTMLAttributes, ReactNode, SelectHTMLAttributes, TextareaHTMLAttributes } from "react";
import { cn } from "@/lib/cn";

const control = "w-full rounded-md border border-line bg-canvas px-3 text-sm text-fg placeholder:text-muted focus:outline-2 focus:outline-link focus:-outline-offset-1 focus:border-transparent disabled:opacity-60";

export function Input({ className, ...rest }: InputHTMLAttributes<HTMLInputElement>) {
  return <input className={cn(control, "h-8", className)} {...rest} />;
}

export function Select({ className, children, ...rest }: SelectHTMLAttributes<HTMLSelectElement>) {
  return <select className={cn(control, "h-8", className)} {...rest}>{children}</select>;
}

export function Textarea({ className, ...rest }: TextareaHTMLAttributes<HTMLTextAreaElement>) {
  return <textarea className={cn(control, "py-2", className)} {...rest} />;
}

export function Checkbox({ label, className, ...rest }: { label: ReactNode } & InputHTMLAttributes<HTMLInputElement>) {
  return (
    <label className={cn("inline-flex items-center gap-2 text-sm cursor-pointer", className)}>
      <input type="checkbox" className="size-3.5 accent-[var(--link)]" {...rest} />
      <span>{label}</span>
    </label>
  );
}

/** A labelled form row: label, optional hint beside it, then the control. */
export function Field({ label, hint, children, className }: { label: ReactNode; hint?: ReactNode; children: ReactNode; className?: string }) {
  return (
    <label className={cn("block", className)}>
      <span className="mb-1 flex flex-wrap items-baseline gap-x-2 text-sm font-semibold">
        {label}
        {hint && <span className="text-xs font-normal text-muted">{hint}</span>}
      </span>
      {children}
    </label>
  );
}
