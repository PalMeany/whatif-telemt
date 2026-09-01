import * as React from "react";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { EmptyState, StatusDot } from "@/components/ui/feedback";
import { cn, formatCount } from "@/lib/utils";

/**
 * Renders an arbitrary Control API object as label/value rows.
 *
 * The Control API returns rich, endpoint-specific payloads that grow with the
 * proxy. Hand-writing a field list per endpoint would mean a panel that silently
 * hides whatever telemt added last release, so scalar fields are rendered
 * generically and only the fields worth highlighting get bespoke treatment.
 */
export function AutoFields({
  value,
  className,
  columns = 2,
  exclude = [],
}: {
  value: unknown;
  className?: string;
  columns?: 1 | 2 | 3;
  exclude?: string[];
}) {
  const rows = React.useMemo(() => flattenScalars(value, exclude), [value, exclude]);
  if (rows.length === 0) {
    return <EmptyState title="No fields" description="This endpoint returned no scalar values." />;
  }
  return (
    <dl
      className={cn(
        "grid gap-x-6",
        columns === 1 ? "grid-cols-1" : columns === 3 ? "sm:grid-cols-3" : "sm:grid-cols-2",
        className,
      )}
    >
      {rows.map((row) => (
        <div
          key={row.path}
          className="flex items-baseline justify-between gap-4 border-b border-[var(--border)]/70 py-1.5 last:border-b-0"
        >
          <dt
            className="max-w-[55%] shrink-0 truncate text-[12px] text-muted-foreground"
            title={row.path}
          >
            {row.label}
          </dt>
          {/* The value truncates, not the label: a config hash is 64 characters
              and squeezing the label out of the row makes the whole line
              unreadable. The full value stays available as a tooltip. */}
          <dd
            className={cn("min-w-0 flex-1 truncate text-right text-[13px]", row.mono && "tabular")}
            title={row.display}
          >
            {row.display}
          </dd>
        </div>
      ))}
    </dl>
  );
}

/** Renders an array of uniform objects as a table with inferred columns. */
export function AutoTable({
  rows,
  emptyTitle = "No rows",
  emptyDescription,
  highlight,
  maxColumns = 9,
}: {
  rows: unknown;
  emptyTitle?: string;
  emptyDescription?: string;
  highlight?: (row: Record<string, unknown>) => "ok" | "warn" | "danger" | "idle" | undefined;
  maxColumns?: number;
}) {
  const list = Array.isArray(rows) ? (rows as Record<string, unknown>[]) : [];
  const columns = React.useMemo(() => {
    const seen: string[] = [];
    for (const row of list) {
      if (!row || typeof row !== "object") continue;
      for (const key of Object.keys(row)) {
        const value = row[key];
        if (value !== null && typeof value === "object" && !Array.isArray(value)) continue;
        if (!seen.includes(key)) seen.push(key);
      }
    }
    return seen.slice(0, maxColumns);
  }, [list, maxColumns]);

  if (list.length === 0 || columns.length === 0) {
    return <EmptyState title={emptyTitle} description={emptyDescription} />;
  }

  return (
    <Table>
      <TableHeader>
        <TableRow>
          {highlight ? <TableHead className="w-8" /> : null}
          {columns.map((column) => (
            <TableHead
              key={column}
              className={isNumericColumn(list, column) ? "text-right" : undefined}
            >
              {humanize(column)}
            </TableHead>
          ))}
        </TableRow>
      </TableHeader>
      <TableBody>
        {list.map((row, index) => (
          <TableRow key={index}>
            {highlight ? (
              <TableCell>
                <StatusDot state={highlight(row) ?? "idle"} />
              </TableCell>
            ) : null}
            {columns.map((column) => {
              const numeric = isNumericColumn(list, column);
              return (
                <TableCell
                  key={column}
                  className={cn(numeric && "tabular text-right", "max-w-[22rem] truncate")}
                  title={renderCell(row[column])}
                >
                  {renderCell(row[column])}
                </TableCell>
              );
            })}
          </TableRow>
        ))}
      </TableBody>
    </Table>
  );
}

type ScalarRow = { path: string; label: string; display: string; mono: boolean };

/** Walks an object tree and collects every scalar leaf. */
function flattenScalars(value: unknown, exclude: string[], prefix = ""): ScalarRow[] {
  if (value === null || value === undefined) return [];
  if (typeof value !== "object") {
    return [
      {
        path: prefix || "value",
        label: humanize(prefix.split(".").pop() ?? "value"),
        display: renderCell(value),
        mono: typeof value === "number",
      },
    ];
  }
  if (Array.isArray(value)) {
    const scalars = value.filter((item) => item === null || typeof item !== "object");
    if (scalars.length === value.length && value.length > 0) {
      return [
        {
          path: prefix,
          label: humanize(prefix.split(".").pop() ?? "list"),
          display: scalars.map((item) => renderCell(item)).join(", "),
          mono: true,
        },
      ];
    }
    return [];
  }
  const rows: ScalarRow[] = [];
  for (const [key, child] of Object.entries(value as Record<string, unknown>)) {
    const path = prefix ? `${prefix}.${key}` : key;
    if (exclude.includes(key) || exclude.includes(path)) continue;
    rows.push(...flattenScalars(child, exclude, path));
  }
  return rows;
}

/** Turns a snake_case field name into a readable label. */
export function humanize(key: string): string {
  return key
    .replace(/_/g, " ")
    .replace(/\b(ms|id|ip|tls|dc|me|nat|stun|rtt|cpu|api|url)\b/gi, (match) => match.toUpperCase())
    .replace(/^./, (character) => character.toUpperCase());
}

/** Renders a scalar for display, keeping large numbers readable. */
export function renderCell(value: unknown): string {
  if (value === null || value === undefined) return "—";
  if (typeof value === "boolean") return value ? "yes" : "no";
  if (typeof value === "number") {
    return Number.isInteger(value) ? formatCount(value) : value.toFixed(3);
  }
  if (Array.isArray(value)) return value.map((item) => renderCell(item)).join(", ");
  if (typeof value === "object") return JSON.stringify(value);
  return String(value);
}

/** True when every present value in the column is numeric. */
function isNumericColumn(rows: Record<string, unknown>[], column: string): boolean {
  let seen = false;
  for (const row of rows) {
    const value = row[column];
    if (value === null || value === undefined) continue;
    if (typeof value !== "number") return false;
    seen = true;
  }
  return seen;
}
