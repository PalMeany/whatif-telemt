import { Badge } from "@/components/ui/badge";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { AutoFields, AutoTable } from "@/components/auto";
import { EdgeGate } from "@/components/edge-gate";
import { PageSection, QueryState, RawJson, RefreshButton, SectionCard } from "@/components/page";
import { useControl } from "@/hooks/use-control";
import { edgeData, type EdgeEnvelope } from "@/lib/edge";

type WhitelistPayload = { enabled: boolean; entries_total: number; entries: string[] };

type FingerprintsPayload = {
  limit?: number;
  retention_secs?: number;
  capacity?: number;
  dropped_total?: number;
  parse_error_total?: number;
  by_fingerprint?: unknown[];
  by_ip?: unknown[];
  by_cidr?: unknown[];
  by_user?: unknown[];
};

export default function SecurityPage() {
  const posture = useControl<Record<string, unknown>>("/v1/security/posture", {
    refetchInterval: 30_000,
  });
  const whitelist = useControl<WhitelistPayload>("/v1/security/whitelist", {
    refetchInterval: 30_000,
  });
  const fingerprints = useControl<EdgeEnvelope<FingerprintsPayload>>(
    "/v1/runtime/tls-fingerprints?limit=100",
    { refetchInterval: 15_000 },
  );
  const nat = useControl<EdgeEnvelope<Record<string, unknown>>>("/v1/runtime/nat-stun", {
    refetchInterval: 20_000,
  });

  const prints = edgeData(fingerprints.data);

  return (
    <div className="flex flex-col gap-5">
      <PageSection
        description="Control-plane posture, source allowlists, and what DPI-facing fingerprints this node has observed."
        actions={
          <RefreshButton
            onClick={() => {
              void posture.refetch();
              void fingerprints.refetch();
            }}
            busy={posture.isFetching}
          />
        }
      >
        <div className="grid gap-4 xl:grid-cols-2">
          <SectionCard title="Posture">
            <QueryState isLoading={posture.isLoading} error={posture.error}>
              <AutoFields value={posture.data} columns={1} />
            </QueryState>
          </SectionCard>
          <SectionCard
            title="API allowlist"
            description="Source networks permitted to reach this node's Control API."
          >
            <QueryState isLoading={whitelist.isLoading} error={whitelist.error}>
              <div className="flex flex-wrap gap-1.5">
                {(whitelist.data?.entries ?? []).map((entry) => (
                  <Badge key={entry} variant="outline" className="font-mono">
                    {entry}
                  </Badge>
                ))}
                {(whitelist.data?.entries.length ?? 0) === 0 ? (
                  <span className="text-[13px] text-muted-foreground">
                    Empty list — every source address is accepted.
                  </span>
                ) : null}
              </div>
            </QueryState>
          </SectionCard>
        </div>

        <SectionCard
          title="Observed TLS fingerprints"
          description={
            prints
              ? `${prints.capacity ?? 0} slots, ${prints.retention_secs ?? 0}s retention, ${prints.dropped_total ?? 0} dropped, ${prints.parse_error_total ?? 0} parse errors.`
              : "What connected clients and scanners look like on the wire."
          }
          bodyClassName="px-0 pb-0"
        >
          <QueryState isLoading={fingerprints.isLoading} error={fingerprints.error}>
            <div className="px-5 pb-4">
              <EdgeGate
                envelope={fingerprints.data}
                feature="Runtime-edge fingerprints"
                hint="Set server.api.runtime_edge_enabled = true on this node."
              >
                <Tabs defaultValue="fingerprint">
                  <TabsList>
                    <TabsTrigger value="fingerprint">By fingerprint</TabsTrigger>
                    <TabsTrigger value="ip">By address</TabsTrigger>
                    <TabsTrigger value="cidr">By network</TabsTrigger>
                    <TabsTrigger value="user">By user</TabsTrigger>
                  </TabsList>
                  <TabsContent value="fingerprint">
                    <AutoTable
                      rows={prints?.by_fingerprint}
                      emptyTitle="Nothing observed yet"
                      emptyDescription="Rows appear once clients or scanners reach the listener."
                    />
                  </TabsContent>
                  <TabsContent value="ip">
                    <AutoTable rows={prints?.by_ip} emptyTitle="Nothing observed yet" />
                  </TabsContent>
                  <TabsContent value="cidr">
                    <AutoTable rows={prints?.by_cidr} emptyTitle="Nothing observed yet" />
                  </TabsContent>
                  <TabsContent value="user">
                    <AutoTable rows={prints?.by_user} emptyTitle="Nothing observed yet" />
                  </TabsContent>
                </Tabs>
              </EdgeGate>
            </div>
          </QueryState>
        </SectionCard>

        <SectionCard title="NAT and STUN" description="Reflection cache, backoff, and live servers.">
          <QueryState isLoading={nat.isLoading} error={nat.error}>
            <EdgeGate envelope={nat.data} feature="NAT and STUN reflection">
              <AutoFields value={edgeData(nat.data)} columns={3} />
              <div className="mt-4">
                <RawJson value={edgeData(nat.data)} />
              </div>
            </EdgeGate>
          </QueryState>
        </SectionCard>
      </PageSection>
    </div>
  );
}
