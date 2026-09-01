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
import type { EdgeEnvelope } from "@/lib/edge";
import { formatCount } from "@/lib/utils";

type UpstreamSummary = {
  configured_total?: number;
  healthy_total?: number;
  unhealthy_total?: number;
  direct_total?: number;
  socks4_total?: number;
  socks5_total?: number;
  shadowsocks_total?: number;
};

type UpstreamsPayload = EdgeEnvelope<never> & {
  zero?: Record<string, unknown>;
  summary?: UpstreamSummary;
  upstreams?: unknown[];
};

type QualityPayload = EdgeEnvelope<never> & {
  policy?: Record<string, unknown>;
  counters?: Record<string, unknown>;
  summary?: UpstreamSummary;
  upstreams?: unknown[];
};

export default function UpstreamsPage() {
  const upstreams = useControl<UpstreamsPayload>("/v1/stats/upstreams", {
    refetchInterval: 10_000,
  });
  const quality = useControl<QualityPayload>("/v1/runtime/upstream-quality", {
    refetchInterval: 10_000,
  });

  const summary = upstreams.data?.summary;
  const unhealthy = summary?.unhealthy_total ?? 0;

  return (
    <div className="flex flex-col gap-5">
      <PageSection
        description="Telegram upstream health, policy, and per-endpoint counters."
        actions={
          <RefreshButton
            onClick={() => {
              void upstreams.refetch();
              void quality.refetch();
            }}
            busy={upstreams.isFetching}
          />
        }
      >
        <QueryState isLoading={upstreams.isLoading} error={upstreams.error}>
          <StatGrid>
            <StatCell>
              <Stat label="Configured" value={formatCount(summary?.configured_total)} />
            </StatCell>
            <StatCell>
              <Stat
                label="Healthy"
                value={formatCount(summary?.healthy_total)}
                tone={(summary?.healthy_total ?? 0) === 0 ? "danger" : "default"}
              />
            </StatCell>
            <StatCell>
              <Stat
                label="Unhealthy"
                value={formatCount(unhealthy)}
                tone={unhealthy > 0 ? "warn" : "default"}
              />
            </StatCell>
            <StatCell>
              <Stat
                label="Direct"
                value={formatCount(summary?.direct_total)}
                hint={`${formatCount(summary?.socks5_total)} socks5 · ${formatCount(summary?.shadowsocks_total)} ss`}
              />
            </StatCell>
          </StatGrid>
        </QueryState>

        <SectionCard title="Upstream rows" bodyClassName="px-0 pb-0">
          <QueryState isLoading={upstreams.isLoading} error={upstreams.error}>
            <AutoTable
              rows={upstreams.data?.upstreams ?? quality.data?.upstreams}
              emptyTitle="No upstream rows"
              emptyDescription="Runtime rows appear once the upstream manager has health data."
              highlight={(row) =>
                typeof row.healthy === "boolean" ? (row.healthy ? "ok" : "danger") : undefined
              }
            />
          </QueryState>
        </SectionCard>

        <div className="grid gap-4 xl:grid-cols-2">
          <SectionCard title="Connect counters">
            <QueryState isLoading={upstreams.isLoading} error={upstreams.error}>
              <EdgeGate envelope={upstreams.data} feature="Upstream counters">
                <AutoFields value={upstreams.data?.zero} columns={2} />
              </EdgeGate>
            </QueryState>
          </SectionCard>
          <SectionCard title="Policy">
            <QueryState isLoading={quality.isLoading} error={quality.error}>
              <EdgeGate envelope={quality.data} feature="Upstream quality">
                <AutoFields value={quality.data?.policy} columns={1} />
                <div className="mt-4">
                  <RawJson value={quality.data} />
                </div>
              </EdgeGate>
            </QueryState>
          </SectionCard>
        </div>
      </PageSection>
    </div>
  );
}
