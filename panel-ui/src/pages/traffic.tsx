import { Stat } from "@/components/ui/stat";
import { AutoFields, AutoTable } from "@/components/auto";
import { EdgeGate } from "@/components/edge-gate";
import {
  PageSection,
  QueryState,
  RawJson,
  RefreshButton,
  SectionCard,
  StatCell,
  StatGrid,
} from "@/components/page";
import { useControl } from "@/hooks/use-control";
import { edgeData, type EdgeEnvelope } from "@/lib/edge";
import { formatCount } from "@/lib/utils";

type ConnectionTotals = {
  current_connections?: number;
  current_connections_me?: number;
  current_connections_direct?: number;
  active_users?: number;
};

type ConnectionsPayload = {
  totals?: ConnectionTotals;
  top?: { limit?: number; by_connections?: unknown[]; by_throughput?: unknown[] };
  telemetry?: Record<string, unknown>;
  cache?: Record<string, unknown>;
};

export default function TrafficPage() {
  const connections = useControl<EdgeEnvelope<ConnectionsPayload>>(
    "/v1/runtime/connections/summary",
    { refetchInterval: 5_000 },
  );
  const zero = useControl<Record<string, unknown>>("/v1/stats/zero/all", {
    refetchInterval: 10_000,
  });
  const activeIps = useControl<{ username: string; active_ips: string[] }[]>(
    "/v1/stats/users/active-ips",
    { refetchInterval: 15_000 },
  );

  const payload = edgeData(connections.data);
  const totals = payload?.totals;

  return (
    <div className="flex flex-col gap-5">
      <PageSection
        actions={
          <RefreshButton
            onClick={() => {
              void connections.refetch();
              void zero.refetch();
            }}
            busy={connections.isFetching}
          />
        }
      >
        <QueryState isLoading={connections.isLoading} error={connections.error}>
          <EdgeGate
            envelope={connections.data}
            feature="Runtime-edge connection accounting"
            hint="Set server.api.runtime_edge_enabled = true on this node."
          >
            <StatGrid>
              <StatCell>
                <Stat
                  label="Active connections"
                  value={formatCount(totals?.current_connections)}
                  hint={`${formatCount(totals?.active_users)} users with traffic`}
                />
              </StatCell>
              <StatCell>
                <Stat
                  label="Via Middle-End"
                  value={formatCount(totals?.current_connections_me)}
                />
              </StatCell>
              <StatCell>
                <Stat label="Direct to DC" value={formatCount(totals?.current_connections_direct)} />
              </StatCell>
              <StatCell>
                <Stat
                  label="Leaderboard depth"
                  value={formatCount(payload?.top?.limit)}
                  hint="server.api.runtime_edge_top_n"
                />
              </StatCell>
            </StatGrid>
          </EdgeGate>
        </QueryState>
      </PageSection>

      <EdgeGate envelope={connections.data} feature="Runtime-edge leaderboards">
        <div className="grid gap-4 xl:grid-cols-2">
          <SectionCard title="Top users by connections" bodyClassName="px-0 pb-0">
            <AutoTable
              rows={payload?.top?.by_connections}
              emptyTitle="No connections yet"
              emptyDescription="The leaderboard fills once clients connect."
            />
          </SectionCard>
          <SectionCard title="Top users by throughput" bodyClassName="px-0 pb-0">
            <AutoTable
              rows={payload?.top?.by_throughput}
              emptyTitle="No throughput yet"
              emptyDescription="The leaderboard fills once clients move data."
            />
          </SectionCard>
        </div>
      </EdgeGate>

      <SectionCard title="Active source addresses" bodyClassName="px-0 pb-0">
        <QueryState
          isLoading={activeIps.isLoading}
          error={activeIps.error}
          isEmpty={(activeIps.data?.length ?? 0) === 0}
          emptyTitle="No user currently holds a connection"
        >
          <AutoTable
            rows={(activeIps.data ?? []).map((row) => ({
              username: row.username,
              addresses: row.active_ips.length,
              list: row.active_ips.join(", "),
            }))}
          />
        </QueryState>
      </SectionCard>

      <SectionCard title="Zero-cost counters" description="Full counter surface of this node.">
        <QueryState isLoading={zero.isLoading} error={zero.error}>
          <AutoFields value={zero.data} columns={3} />
          <div className="mt-4">
            <RawJson value={zero.data} />
          </div>
        </QueryState>
      </SectionCard>
    </div>
  );
}
