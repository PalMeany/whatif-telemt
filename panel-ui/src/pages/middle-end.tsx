import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { AutoFields, AutoTable } from "@/components/auto";
import { EdgeGate } from "@/components/edge-gate";
import { PageSection, QueryState, RawJson, RefreshButton, SectionCard } from "@/components/page";
import { useControl } from "@/hooks/use-control";
import { edgeData, type EdgeEnvelope } from "@/lib/edge";

type Payload = Record<string, unknown>;

type WritersPayload = { summary?: Payload; writers?: unknown[] };
type DcsPayload = { dcs?: unknown[] };

export default function MiddleEndPage() {
  const poolState = useControl<EdgeEnvelope<Payload>>("/v1/runtime/me-pool-state", {
    refetchInterval: 10_000,
  });
  const quality = useControl<EdgeEnvelope<Payload>>("/v1/runtime/me-quality", {
    refetchInterval: 10_000,
  });
  const writers = useControl<WritersPayload & EdgeEnvelope<Payload>>("/v1/stats/me-writers", {
    refetchInterval: 10_000,
  });
  const dcs = useControl<DcsPayload & EdgeEnvelope<Payload>>("/v1/stats/dcs", {
    refetchInterval: 10_000,
  });
  const minimal = useControl<EdgeEnvelope<Payload>>("/v1/stats/minimal/all", {
    refetchInterval: 15_000,
  });
  const selftest = useControl<EdgeEnvelope<Payload>>("/v1/runtime/me-selftest", {
    refetchInterval: 30_000,
  });

  return (
    <div className="flex flex-col gap-5">
      <PageSection
        description="Generation state, writer contour, and per-datacentre coverage of the Middle-End pool."
        actions={
          <RefreshButton
            onClick={() => {
              void poolState.refetch();
              void quality.refetch();
              void writers.refetch();
              void dcs.refetch();
            }}
            busy={poolState.isFetching}
          />
        }
      >
        <Tabs defaultValue="pool">
          <TabsList>
            <TabsTrigger value="pool">Pool</TabsTrigger>
            <TabsTrigger value="writers">Writers</TabsTrigger>
            <TabsTrigger value="dcs">Datacentres</TabsTrigger>
            <TabsTrigger value="quality">Quality</TabsTrigger>
            <TabsTrigger value="selftest">Self-test</TabsTrigger>
            <TabsTrigger value="snapshot">Snapshot</TabsTrigger>
          </TabsList>

          <TabsContent value="pool">
            <SectionCard title="Pool state">
              <QueryState isLoading={poolState.isLoading} error={poolState.error}>
                <EdgeGate envelope={poolState.data} feature="The Middle-End pool">
                  <AutoFields value={edgeData(poolState.data)} columns={3} />
                  <div className="mt-4">
                    <RawJson value={edgeData(poolState.data)} />
                  </div>
                </EdgeGate>
              </QueryState>
            </SectionCard>
          </TabsContent>

          <TabsContent value="writers">
            <SectionCard title="Writer coverage" bodyClassName="px-0 pb-0">
              <QueryState isLoading={writers.isLoading} error={writers.error}>
                <div className="px-5 pb-4">
                  <EdgeGate envelope={writers.data} feature="Middle-End writers">
                    <AutoFields value={writers.data?.summary} columns={3} />
                  </EdgeGate>
                </div>
                <AutoTable
                  rows={writers.data?.writers}
                  emptyTitle="No writer rows"
                  highlight={(row) =>
                    typeof row.alive === "boolean" ? (row.alive ? "ok" : "danger") : undefined
                  }
                />
              </QueryState>
            </SectionCard>
          </TabsContent>

          <TabsContent value="dcs">
            <SectionCard title="Datacentre status" bodyClassName="px-0 pb-0">
              <QueryState isLoading={dcs.isLoading} error={dcs.error}>
                <div className="px-5 pb-4">
                  <EdgeGate envelope={dcs.data} feature="Per-datacentre status">
                    <span className="text-[13px] text-muted-foreground">
                      {(dcs.data?.dcs?.length ?? 0)} datacentre rows reported.
                    </span>
                  </EdgeGate>
                </div>
                <AutoTable rows={dcs.data?.dcs} emptyTitle="No datacentre rows" />
              </QueryState>
            </SectionCard>
          </TabsContent>

          <TabsContent value="quality">
            <SectionCard title="Lifecycle and route quality">
              <QueryState isLoading={quality.isLoading} error={quality.error}>
                <EdgeGate envelope={quality.data} feature="Middle-End quality">
                  <AutoFields value={edgeData(quality.data)} columns={3} />
                  <div className="mt-4">
                    <RawJson value={edgeData(quality.data)} />
                  </div>
                </EdgeGate>
              </QueryState>
            </SectionCard>
          </TabsContent>

          <TabsContent value="selftest">
            <SectionCard
              title="Self-test"
              description="KDF, clock skew, address family, PID, and SOCKS BND observations."
            >
              <QueryState isLoading={selftest.isLoading} error={selftest.error}>
                <EdgeGate envelope={selftest.data} feature="The Middle-End self-test">
                  <AutoFields value={edgeData(selftest.data)} columns={2} />
                  <div className="mt-4">
                    <RawJson value={edgeData(selftest.data)} />
                  </div>
                </EdgeGate>
              </QueryState>
            </SectionCard>
          </TabsContent>

          <TabsContent value="snapshot">
            <SectionCard
              title="Minimal runtime snapshot"
              description="Cached aggregate served by /v1/stats/minimal/all."
            >
              <QueryState isLoading={minimal.isLoading} error={minimal.error}>
                <EdgeGate envelope={minimal.data} feature="The minimal runtime snapshot">
                  <AutoFields value={edgeData(minimal.data) ?? minimal.data} columns={3} />
                  <div className="mt-4">
                    <RawJson value={minimal.data} />
                  </div>
                </EdgeGate>
              </QueryState>
            </SectionCard>
          </TabsContent>
        </Tabs>
      </PageSection>
    </div>
  );
}
