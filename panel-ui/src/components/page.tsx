import * as React from "react";
import { RefreshCw } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle, CardDescription } from "@/components/ui/card";
import { EmptyState, Notice, Skeleton } from "@/components/ui/feedback";
import { cn } from "@/lib/utils";

/** Section heading with an optional action cluster on the right. */
export function PageSection({
  title,
  description,
  actions,
  children,
  className,
}: {
  title?: string;
  description?: string;
  actions?: React.ReactNode;
  children: React.ReactNode;
  className?: string;
}) {
  return (
    <section className={cn("flex flex-col gap-3", className)}>
      {title || actions ? (
        <div className="flex flex-wrap items-end justify-between gap-3">
          <div>
            {title ? (
              <h2 className="text-[13px] font-semibold uppercase tracking-[0.1em] text-muted-foreground">
                {title}
              </h2>
            ) : null}
            {description ? (
              <p className="mt-1 text-[13px] text-muted-foreground">{description}</p>
            ) : null}
          </div>
          {actions ? <div className="flex flex-wrap items-center gap-2">{actions}</div> : null}
        </div>
      ) : null}
      {children}
    </section>
  );
}

/** Card wrapper with a consistent header shape. */
export function SectionCard({
  title,
  description,
  actions,
  children,
  bodyClassName,
  className,
}: {
  title?: React.ReactNode;
  description?: React.ReactNode;
  actions?: React.ReactNode;
  children: React.ReactNode;
  bodyClassName?: string;
  className?: string;
}) {
  return (
    <Card className={className}>
      {title || actions ? (
        <CardHeader className="flex-row items-start justify-between gap-3 space-y-0">
          <div className="min-w-0">
            {title ? <CardTitle>{title}</CardTitle> : null}
            {description ? <CardDescription>{description}</CardDescription> : null}
          </div>
          {actions ? <div className="flex shrink-0 items-center gap-2">{actions}</div> : null}
        </CardHeader>
      ) : null}
      <CardContent className={cn(title || actions ? "pt-0" : "pt-5", bodyClassName)}>
        {children}
      </CardContent>
    </Card>
  );
}

/** Renders loading, error, and empty states around a query result. */
export function QueryState({
  isLoading,
  error,
  isEmpty,
  emptyTitle = "Nothing to show",
  emptyDescription,
  skeletonRows = 3,
  children,
}: {
  isLoading: boolean;
  error: unknown;
  isEmpty?: boolean;
  emptyTitle?: string;
  emptyDescription?: string;
  skeletonRows?: number;
  children: React.ReactNode;
}) {
  if (isLoading) {
    return (
      <div className="flex flex-col gap-2 py-1">
        {Array.from({ length: skeletonRows }).map((_, index) => (
          <Skeleton key={index} className="h-9 w-full" />
        ))}
      </div>
    );
  }
  if (error) {
    return (
      <Notice tone="danger" title="Could not load this view">
        {error instanceof Error ? error.message : "Unknown failure"}
      </Notice>
    );
  }
  if (isEmpty) {
    return <EmptyState title={emptyTitle} description={emptyDescription} />;
  }
  return <>{children}</>;
}

/** Refresh control shared by every polling view. */
export function RefreshButton({
  onClick,
  busy,
  label = "Refresh",
}: {
  onClick: () => void;
  busy?: boolean;
  label?: string;
}) {
  return (
    <Button variant="outline" size="sm" onClick={onClick} disabled={busy}>
      <RefreshCw className={busy ? "animate-spin" : undefined} />
      {label}
    </Button>
  );
}

/** Grid of statistic tiles with a consistent responsive rhythm. */
export function StatGrid({
  children,
  className,
}: {
  children: React.ReactNode;
  className?: string;
}) {
  return (
    <div
      className={cn(
        "grid grid-cols-2 gap-px overflow-hidden rounded-[var(--radius-panel)] border border-[var(--border)] bg-[var(--border)] sm:grid-cols-3 xl:grid-cols-4",
        className,
      )}
    >
      {children}
    </div>
  );
}

/** One cell inside a `StatGrid`. */
export function StatCell({ children }: { children: React.ReactNode }) {
  return <div className="bg-[var(--card)] px-4 py-3.5">{children}</div>;
}

/** Collapsible raw payload, for endpoints richer than the page renders. */
export function RawJson({ value, label = "Raw payload" }: { value: unknown; label?: string }) {
  return (
    <details className="group rounded-md border border-[var(--border)]">
      <summary className="cursor-pointer select-none px-3 py-2 text-[12px] text-muted-foreground transition-colors hover:text-[var(--foreground)]">
        {label}
      </summary>
      <pre className="max-h-96 overflow-auto border-t border-[var(--border)] px-3 py-2.5 font-mono text-[11.5px] leading-relaxed">
        {JSON.stringify(value, null, 2)}
      </pre>
    </details>
  );
}
