import * as React from "react";
import { cn } from "@/lib/utils";

/**
 * One headline number.
 *
 * The value carries the visual weight, the label sits above it in small caps,
 * and any qualifier sits beneath in muted text — one hierarchy repeated across
 * every page so a reader learns it once.
 */
export function Stat({
  label,
  value,
  hint,
  className,
  tone,
}: {
  label: string;
  value: React.ReactNode;
  hint?: React.ReactNode;
  className?: string;
  tone?: "default" | "danger" | "warn" | "ok";
}) {
  const toneClass =
    tone === "danger"
      ? "text-[var(--danger)]"
      : tone === "warn"
        ? "text-[var(--warn)]"
        : tone === "ok"
          ? "text-[var(--ok)]"
          : "";
  return (
    <div className={cn("flex flex-col gap-1", className)}>
      <div className="text-[11px] font-medium uppercase tracking-[0.08em] text-muted-foreground">
        {label}
      </div>
      <div className={cn("tabular text-2xl font-semibold leading-none tracking-tight", toneClass)}>
        {value}
      </div>
      {hint ? <div className="text-[12px] text-muted-foreground">{hint}</div> : null}
    </div>
  );
}

/** Compact label/value row used inside detail panels. */
export function Field({
  label,
  value,
  mono = false,
  className,
}: {
  label: string;
  value: React.ReactNode;
  mono?: boolean;
  className?: string;
}) {
  return (
    <div className={cn("flex items-baseline justify-between gap-4 py-1.5", className)}>
      <span className="shrink-0 text-[12px] text-muted-foreground">{label}</span>
      <span className={cn("min-w-0 truncate text-right text-[13px]", mono && "tabular")}>
        {value}
      </span>
    </div>
  );
}
