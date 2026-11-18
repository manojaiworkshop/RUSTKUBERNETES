import type { ReactNode } from "react";
import { cn } from "@/lib/cn";

/** A plain table for short lists (columns of a table, roles, models). Scrolls sideways on narrow screens. */
export function SimpleTable({ head, children, className }: { head: ReactNode[]; children: ReactNode; className?: string }) {
  return (
    <div className={cn("overflow-x-auto", className)}>
      <table className="w-full border-collapse text-left text-sm">
        <thead>
          <tr>{head.map((h, i) => <th key={i} className="whitespace-nowrap border-b border-line bg-subtle px-3 py-2 text-xs font-semibold">{h}</th>)}</tr>
        </thead>
        <tbody className="[&_td]:border-b [&_td]:border-line [&_td]:px-3 [&_td]:py-1.5 [&_td]:align-middle [&_tr:last-child_td]:border-b-0 [&_tr:hover_td]:bg-subtle">{children}</tbody>
      </table>
    </div>
  );
}
