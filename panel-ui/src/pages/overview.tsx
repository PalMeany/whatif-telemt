import * as React from "react";
import { Link } from "react-router-dom";
import { TriangleAlert, ArrowUpRight, CircleCheckBig } from "lucide-react";
import { useQuery } from "@tanstack/react-query";
import { Badge } from "@/components/ui/badge";
import { Stat } from "@/components/ui/stat";
import { MeterBar, Sparkline } from "@/components/ui/sparkline";
import { StatusDot } from "@/components/ui/feedback";
import { PageSection, QueryState, RefreshButton, SectionCard, StatCell, StatGrid } from "@/components/page";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { panelApi } from "@/lib/api";
import { useControl } from "@/hooks/use-control";
import { useNodes } from "@/hooks/use-node";
import { formatCount, formatDuration } from "@/lib/utils";
import type { HealthReadyData, OverviewRow, SummaryData } from "@/lib/types";

export default function OverviewPage() {
  const { node, nodeId } = useNodes();
  const summary = useControl<SummaryData>("/v1/stats/summary", { refetchInterval: 5_000 });
  const ready = useControl<HealthReadyData>("/v1/health/ready", { refetchInterval: 10_000 });
  const history = useConnectionHistory(summary.data?.connections_total, nodeId);

  const fleet = useQuery({
    queryKey: ["overview"],
    queryFn: () => panelApi<{ nodes: OverviewRow[] }>("/overview"),
    refetchInterval: 30_000,
  });

  const badTotal = summary.data?.connections_bad_total ?? 0;
  const allTotal = summary.data?.connections_total ?? 0;

  return (
    <div className="flex flex-col gap-6">
      <PageSection
        title={node?.name ?? nodeId}
        actions={
          <RefreshButton
            onClick={() => {
              void summary.refetch();
              void ready.refetch();
            }}
            busy={summary.isFetching}
          />
        }
      >
        <QueryState isLoading={summary.isLoading} error={summary.error}>
          <StatGrid>
            <StatCell>
              <Stat
                label="Uptime"
                value={formatDuration(summary.data?.uptime_seconds)}
                hint={ready.data?.admission_open ? "admission open" : "admission closed"}
              />
            </StatCell>
            <StatCell>
              <Stat label="Connections" value={formatCount(allTotal)} hint="since start" />
            </StatCell>
            <StatCell>
              <Stat
                label="Rejected"
                value={formatCount(badTotal)}
                tone={badTotal > 0 ? "warn" : "default"}
                hint={
                  allTotal > 0 ? `${((badTotal / allTotal) * 100).toFixed(2)}% of all` : "none yet"
                }
              />
            </StatCell>
            <StatCell>
              <Stat
                label="Users"
                value={formatCount(summary.data?.configured_users)}
                hint="configured"
              />
            </StatCell>
          </StatGrid>
        </QueryState>
      </PageSection>

      <div className="grid gap-4 lg:grid-cols-3">
        <SectionCard
          className="lg:col-span-2"
          title="Connection trend"
          description="Sampled from the running counter while this page is open."
        >
          <div className="text-[var(--foreground)]">
            <Sparkline values={history} height={72} ariaLabel="connections over time" />
          </div>
          <div className="mt-2 flex items-center justify-between text-[11px] text-muted-foreground">
            <span>{history.length < 2 ? "collecting samples" : `${history.length} samples`}</span>
            <span className="tabular">{formatCount(allTotal)} total</span>
          </div>
        </SectionCard>

        <SectionCard title="Readiness">
          <QueryState isLoading={ready.isLoading} error={ready.error} skeletonRows={2}>
            <div className="flex flex-col gap-3">
              <div className="flex items-center gap-2">
                <StatusDot state={ready.data?.ready ? "ok" : "danger"} />
                <span className="text-sm font-medium">
                  {ready.data?.ready ? "Ready" : "Not ready"}
                </span>
                {ready.data?.reason ? (
                  <Badge variant="outline">{ready.data.reason}</Badge>
                ) : null}
              </div>
              <div>
                <div className="mb-1.5 flex items-baseline justify-between text-[12px]">
                  <span className="text-muted-foreground">Healthy upstreams</span>
                  <span className="tabular">
                    {ready.data?.healthy_upstreams ?? 0}/{ready.data?.total_upstreams ?? 0}
                  </span>
                </div>
                <MeterBar
                  value={ready.data?.healthy_upstreams ?? 0}
                  total={ready.data?.total_upstreams ?? 0}
                />
              </div>
              <Link
                to="/upstreams"
                className="inline-flex items-center gap-1 text-[12px] text-muted-foreground underline-offset-4 hover:text-[var(--foreground)] hover:underline"
              >
                Upstream detail <ArrowUpRight className="size-3" />
              </Link>
            </div>
          </QueryState>
        </SectionCard>
      </div>

      <div className="grid gap-4 lg:grid-cols-2">
        <SectionCard
          title="Rejected connections by class"
          description="Where refused handshakes are being classified."
        >
          <ClassBreakdown rows={summary.data?.connections_bad_by_class ?? []} total={badTotal} />
        </SectionCard>
        <SectionCard
          title="Handshake failures by class"
          description={`${formatCount(summary.data?.handshake_timeouts_total)} timeouts recorded.`}
        >
          <ClassBreakdown
            rows={summary.data?.handshake_failures_by_class ?? []}
            total={(summary.data?.handshake_failures_by_class ?? []).reduce(
              (sum, row) => sum + row.total,
              0,
            )}
          />
        </SectionCard>
      </div>

      <PageSection
        title="Fleet"
        description="Every node this panel can reach, refreshed every 30 seconds."
        actions={<RefreshButton onClick={() => void fleet.refetch()} busy={fleet.isFetching} />}
      >
        <SectionCard bodyClassName="px-0 pb-0">
          <QueryState
            isLoading={fleet.isLoading}
            error={fleet.error}
            isEmpty={(fleet.data?.nodes.length ?? 0) === 0}
            emptyTitle="No nodes"
          >
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead>Node</TableHead>
                  <TableHead>State</TableHead>
                  <TableHead className="text-right">Uptime</TableHead>
                  <TableHead className="text-right">Connections</TableHead>
                  <TableHead className="text-right">Rejected</TableHead>
                  <TableHead className="text-right">Users</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {(fleet.data?.nodes ?? []).map((row) => {
                  const readiness = row.ready;
                  return (
                    <TableRow key={row.node_id}>
                      <TableCell className="font-medium">
                        {row.node_name}
                        <div className="font-mono text-[11px] text-muted-foreground">
                          {row.node_id}
                        </div>
                      </TableCell>
                      <TableCell>
                        {row.reachable ? (
                          <StatusDot
                            state={readiness?.ready === false ? "warn" : "ok"}
                            label={readiness?.ready === false ? "not ready" : "ready"}
                          />
                        ) : (
                          <span className="inline-flex items-center gap-1.5 text-[var(--danger)]">
                            <TriangleAlert className="size-3.5" />
                            <span className="truncate text-[12px]">
                              {row.error ?? "unreachable"}
                            </span>
                          </span>
                        )}
                      </TableCell>
                      <TableCell className="tabular text-right">
                        {formatDuration(row.summary?.uptime_seconds)}
                      </TableCell>
                      <TableCell className="tabular text-right">
                        {formatCount(row.summary?.connections_total)}
                      </TableCell>
                      <TableCell className="tabular text-right">
                        {formatCount(row.summary?.connections_bad_total)}
                      </TableCell>
                      <TableCell className="tabular text-right">
                        {formatCount(row.summary?.configured_users)}
                      </TableCell>
                    </TableRow>
                  );
                })}
              </TableBody>
            </Table>
          </QueryState>
        </SectionCard>
      </PageSection>
    </div>
  );
}

function ClassBreakdown({
  rows,
  total,
}: {
  rows: { class: string; total: number }[];
  total: number;
}) {
  if (rows.length === 0) {
    return (
      <div className="flex items-center gap-2 py-2 text-[13px] text-muted-foreground">
        <CircleCheckBig className="size-4 text-[var(--ok)]" />
        Nothing recorded.
      </div>
    );
  }
  const sorted = [...rows].sort((left, right) => right.total - left.total);
  return (
    <div className="flex flex-col gap-2.5">
      {sorted.map((row) => (
        <div key={row.class} className="flex flex-col gap-1">
          <div className="flex items-baseline justify-between gap-3 text-[13px]">
            <span className="truncate font-mono text-[12px]">{row.class}</span>
            <span className="tabular shrink-0">{formatCount(row.total)}</span>
          </div>
          <MeterBar value={row.total} total={total} />
        </div>
      ))}
    </div>
  );
}

/** Keeps a short in-memory series so the trend line has something to draw. */
function useConnectionHistory(total: number | undefined, nodeId: string): number[] {
  const [series, setSeries] = React.useState<number[]>([]);
  const lastNode = React.useRef(nodeId);
  React.useEffect(() => {
    if (lastNode.current !== nodeId) {
      lastNode.current = nodeId;
      setSeries([]);
    }
  }, [nodeId]);
  React.useEffect(() => {
    if (total === undefined) return;
    setSeries((previous) => {
      if (previous.length > 0 && previous[previous.length - 1] === total) return previous;
      return [...previous, total].slice(-60);
    });
  }, [total]);
  return series;
}
