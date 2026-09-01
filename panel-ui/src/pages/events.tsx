import * as React from "react";
import { Badge } from "@/components/ui/badge";
import { Input } from "@/components/ui/input";
import { PageSection, QueryState, RefreshButton, SectionCard } from "@/components/page";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { EdgeGate } from "@/components/edge-gate";
import { useControl } from "@/hooks/use-control";
import { edgeData, type EdgeEnvelope } from "@/lib/edge";
import { formatRelative, formatTime } from "@/lib/utils";

type RuntimeEvent = {
  seq: number;
  ts_epoch_secs: number;
  event_type: string;
  context: string;
};

type EventsPayload = {
  capacity?: number;
  dropped_total?: number;
  events?: RuntimeEvent[];
};

export default function EventsPage() {
  const [limit, setLimit] = React.useState(100);
  const [filter, setFilter] = React.useState("");
  const events = useControl<EdgeEnvelope<EventsPayload>>(
    `/v1/runtime/events/recent?limit=${limit}`,
    { refetchInterval: 5_000 },
  );

  const payload = edgeData(events.data);
  const rows = React.useMemo(() => {
    const list = [...(payload?.events ?? [])].reverse();
    const needle = filter.trim().toLowerCase();
    if (!needle) return list;
    return list.filter((row) =>
      `${row.event_type} ${row.context}`.toLowerCase().includes(needle),
    );
  }, [payload, filter]);

  return (
    <div className="flex flex-col gap-5">
      <PageSection
        description={
          payload
            ? `Ring buffer of ${payload.capacity ?? 0} records; ${payload.dropped_total ?? 0} dropped since start.`
            : "Control-plane events recorded by the node's ring buffer."
        }
        actions={
          <>
            <Input
              value={filter}
              onChange={(event) => setFilter(event.target.value)}
              placeholder="Filter events"
              className="h-9 w-52"
              aria-label="Filter events"
            />
            <Input
              value={limit}
              onChange={(event) => setLimit(Math.min(1000, Number(event.target.value) || 100))}
              inputMode="numeric"
              className="h-9 w-20 tabular"
              aria-label="Event limit"
            />
            <RefreshButton onClick={() => void events.refetch()} busy={events.isFetching} />
          </>
        }
      >
        <QueryState isLoading={events.isLoading} error={events.error}>
          <EdgeGate
            envelope={events.data}
            feature="Runtime-edge events"
            hint="Set server.api.runtime_edge_enabled = true on this node."
          >
            <SectionCard bodyClassName="px-0 pb-0">
              {rows.length === 0 ? (
                <div className="px-5 py-12 text-center text-[13px] text-muted-foreground">
                  {filter ? "No event matches that filter." : "No events recorded yet."}
                </div>
              ) : (
                <Table>
                  <TableHeader>
                    <TableRow>
                      <TableHead className="w-14 text-right">Seq</TableHead>
                      <TableHead className="w-44">Time</TableHead>
                      <TableHead className="w-56">Type</TableHead>
                      <TableHead>Context</TableHead>
                    </TableRow>
                  </TableHeader>
                  <TableBody>
                    {rows.map((row) => (
                      <TableRow key={row.seq}>
                        <TableCell className="tabular text-right text-muted-foreground">
                          {row.seq}
                        </TableCell>
                        <TableCell className="tabular whitespace-nowrap text-[12px]">
                          <div>{formatTime(row.ts_epoch_secs)}</div>
                          <div className="text-muted-foreground">
                            {formatRelative(row.ts_epoch_secs)}
                          </div>
                        </TableCell>
                        <TableCell>
                          <Badge variant="outline" className="font-mono">
                            {row.event_type}
                          </Badge>
                        </TableCell>
                        <TableCell className="font-mono text-[12px] leading-relaxed">
                          {row.context || "—"}
                        </TableCell>
                      </TableRow>
                    ))}
                  </TableBody>
                </Table>
              )}
            </SectionCard>
          </EdgeGate>
        </QueryState>
      </PageSection>
    </div>
  );
}
