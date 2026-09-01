import * as React from "react";
import { cn } from "@/lib/utils";

/**
 * Monochrome sparkline.
 *
 * Hand-drawn rather than pulled from a charting library: the panel needs one
 * shape, in one colour, at one size, and a library would add far more bundle
 * than the twenty lines it replaces.
 */
export function Sparkline({
  values,
  className,
  height = 36,
  ariaLabel,
}: {
  values: number[];
  className?: string;
  height?: number;
  ariaLabel?: string;
}) {
  const width = 160;
  const path = React.useMemo(() => {
    if (values.length < 2) return null;
    const max = Math.max(...values);
    const min = Math.min(...values);
    const span = max - min || 1;
    const step = width / (values.length - 1);
    const points = values.map((value, index) => {
      const x = index * step;
      const y = height - ((value - min) / span) * (height - 4) - 2;
      return `${x.toFixed(2)},${y.toFixed(2)}`;
    });
    return {
      line: `M${points.join(" L")}`,
      area: `M0,${height} L${points.join(" L")} L${width},${height} Z`,
    };
  }, [values, height]);

  if (!path) {
    return <div className={cn("h-9 w-full rounded bg-[var(--muted)]/40", className)} />;
  }
  return (
    <svg
      viewBox={`0 0 ${width} ${height}`}
      preserveAspectRatio="none"
      className={cn("h-9 w-full", className)}
      role="img"
      aria-label={ariaLabel ?? "trend"}
    >
      <path d={path.area} fill="currentColor" opacity="0.1" />
      <path
        d={path.line}
        fill="none"
        stroke="currentColor"
        strokeWidth="1.5"
        strokeLinejoin="round"
        strokeLinecap="round"
        vectorEffect="non-scaling-stroke"
      />
    </svg>
  );
}

/** Horizontal proportion bar used for class breakdowns. */
export function MeterBar({
  value,
  total,
  className,
}: {
  value: number;
  total: number;
  className?: string;
}) {
  const ratio = total > 0 ? Math.min(1, value / total) : 0;
  return (
    <div
      className={cn("h-1.5 w-full overflow-hidden rounded-full bg-[var(--muted)]", className)}
      role="img"
      aria-label={`${value} of ${total}`}
    >
      <div
        className="h-full rounded-full bg-[var(--foreground)]"
        style={{ width: `${(ratio * 100).toFixed(1)}%` }}
      />
    </div>
  );
}
