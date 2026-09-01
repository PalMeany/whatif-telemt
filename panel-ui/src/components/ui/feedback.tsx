import * as React from "react";
import { TriangleAlert, Check, Copy, Info, LoaderCircle } from "lucide-react";
import { cn, copyText } from "@/lib/utils";
import { Button } from "./button";

/** Inline status note. Colour is the only place the theme leaves monochrome. */
export function Notice({
  tone = "info",
  title,
  children,
  className,
}: {
  tone?: "info" | "warn" | "danger";
  title?: React.ReactNode;
  children?: React.ReactNode;
  className?: string;
}) {
  const Icon = tone === "info" ? Info : TriangleAlert;
  const toneClass =
    tone === "danger"
      ? "border-[color-mix(in_oklab,var(--danger)_35%,transparent)] text-[var(--danger)]"
      : tone === "warn"
        ? "border-[color-mix(in_oklab,var(--warn)_35%,transparent)] text-[var(--warn)]"
        : "border-[var(--border)] text-muted-foreground";
  return (
    <div
      role={tone === "info" ? "status" : "alert"}
      className={cn(
        "flex items-start gap-2.5 rounded-md border bg-[var(--muted)]/50 px-3 py-2.5 text-[13px] leading-snug",
        toneClass,
        className,
      )}
    >
      <Icon className="mt-0.5 size-4 shrink-0" />
      <div className="min-w-0 flex-1">
        {title ? <div className="font-medium">{title}</div> : null}
        {children ? <div className={cn(title && "mt-0.5 opacity-90")}>{children}</div> : null}
      </div>
    </div>
  );
}

/** Full-width loading placeholder that keeps the layout from jumping. */
export function Skeleton({ className }: { className?: string }) {
  return (
    <div
      className={cn("animate-pulse rounded-md bg-[var(--muted)]", className)}
      aria-hidden="true"
    />
  );
}

export function Spinner({ className }: { className?: string }) {
  return <LoaderCircle className={cn("size-4 animate-spin", className)} />;
}

/** Placeholder used when a collection is legitimately empty. */
export function EmptyState({
  icon,
  title,
  description,
  action,
}: {
  icon?: React.ReactNode;
  title: string;
  description?: string;
  action?: React.ReactNode;
}) {
  return (
    <div className="flex flex-col items-center justify-center gap-2 px-6 py-14 text-center">
      {icon ? <div className="text-muted-foreground/60 [&_svg]:size-6">{icon}</div> : null}
      <div className="text-sm font-medium">{title}</div>
      {description ? (
        <div className="max-w-sm text-[13px] text-muted-foreground">{description}</div>
      ) : null}
      {action ? <div className="mt-2">{action}</div> : null}
    </div>
  );
}

/** Copy button that confirms in place instead of firing a toast. */
export function CopyButton({
  value,
  label = "Copy",
  className,
}: {
  value: string;
  label?: string;
  className?: string;
}) {
  const [copied, setCopied] = React.useState(false);
  React.useEffect(() => {
    if (!copied) return;
    const timer = window.setTimeout(() => setCopied(false), 1600);
    return () => window.clearTimeout(timer);
  }, [copied]);
  return (
    <Button
      type="button"
      variant="outline"
      size="xs"
      className={className}
      onClick={async () => setCopied(await copyText(value))}
      aria-label={label}
    >
      {copied ? <Check className="text-[var(--ok)]" /> : <Copy />}
      {copied ? "Copied" : label}
    </Button>
  );
}

/** Monospaced block for secrets, links, and raw payloads. */
export function CodeBlock({
  value,
  className,
  wrap = true,
}: {
  value: string;
  className?: string;
  wrap?: boolean;
}) {
  return (
    <pre
      className={cn(
        "overflow-x-auto rounded-md border border-[var(--border)] bg-[var(--muted)]/60 px-3 py-2.5 font-mono text-[12px] leading-relaxed",
        wrap ? "whitespace-pre-wrap break-all" : "whitespace-pre",
        className,
      )}
    >
      {value}
    </pre>
  );
}

/** Small status dot with an accessible label. */
export function StatusDot({
  state,
  label,
}: {
  state: "ok" | "warn" | "danger" | "idle";
  label?: string;
}) {
  const tone =
    state === "ok"
      ? "bg-[var(--ok)]"
      : state === "warn"
        ? "bg-[var(--warn)]"
        : state === "danger"
          ? "bg-[var(--danger)]"
          : "bg-[var(--muted-foreground)]";
  return (
    <span className="inline-flex items-center gap-1.5">
      <span className={cn("size-1.5 rounded-full", tone)} aria-hidden="true" />
      {label ? <span>{label}</span> : null}
      <span className="sr-only">{state}</span>
    </span>
  );
}
