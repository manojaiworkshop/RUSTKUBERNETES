import type { AnchorHTMLAttributes, ButtonHTMLAttributes, ReactNode } from "react";
import { cn } from "@/lib/cn";
import { Icon, type IconName } from "./icons";

type Variant = "default" | "primary" | "danger" | "ghost";

/** One size everywhere (28 px tall); `block` stretches it, `lg` is for the sign-in form. */
const base = "inline-flex items-center justify-center gap-1.5 h-7 px-3 rounded-md border text-xs font-medium whitespace-nowrap select-none cursor-pointer transition-colors disabled:opacity-50 disabled:cursor-default focus-visible:outline-2 focus-visible:outline-link focus-visible:outline-offset-1";
const variants: Record<Variant, string> = {
  default: "bg-subtle border-line text-fg hover:bg-inset",
  primary: "bg-primary border-transparent text-white hover:bg-primary-hover",
  danger: "bg-subtle border-line text-danger hover:bg-danger hover:text-white hover:border-danger",
  ghost: "bg-transparent border-transparent text-fg hover:bg-inset",
};

interface Common { variant?: Variant; icon?: IconName; block?: boolean; lg?: boolean; children?: ReactNode }

export function buttonClass({ variant = "default", block, lg, className }: Common & { className?: string }) {
  return cn(base, variants[variant], block && "w-full", lg && "h-9 text-sm", className);
}

export function Button({ variant, icon, block, lg, className, children, type = "button", ...rest }: Common & ButtonHTMLAttributes<HTMLButtonElement>) {
  return (
    <button type={type} className={buttonClass({ variant, block, lg, className })} {...rest}>
      {icon && <Icon name={icon} />}
      {children}
    </button>
  );
}

/** A link that looks like a button (downloads, navigation). */
export function ButtonLink({ variant, icon, block, lg, className, children, ...rest }: Common & AnchorHTMLAttributes<HTMLAnchorElement>) {
  return (
    <a className={cn(buttonClass({ variant, block, lg, className }), "hover:no-underline")} {...rest}>
      {icon && <Icon name={icon} />}
      {children}
    </a>
  );
}

/** A square button holding only an icon; `label` is its accessible name and tooltip. */
export function IconButton({ icon, label, variant = "default", className, ...rest }: { icon: IconName; label: string; variant?: Variant } & ButtonHTMLAttributes<HTMLButtonElement>) {
  return (
    <button type="button" title={label} aria-label={label} className={cn(base, variants[variant], "w-7 px-0", className)} {...rest}>
      <Icon name={icon} width={14} height={14} />
    </button>
  );
}
