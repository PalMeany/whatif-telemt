import * as React from "react";
import { Play, RotateCw } from "lucide-react";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogBody,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Label } from "@/components/ui/label";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import { Notice, StatusDot } from "@/components/ui/feedback";
import { Stat } from "@/components/ui/stat";
import { AutoFields, AutoTable } from "@/components/auto";
import {
  PageSection,
  QueryState,
  RawJson,
  RefreshButton,
  SectionCard,
  StatCell,
  StatGrid,
} from "@/components/page";
import { controlData } from "@/lib/api";
import { useControl } from "@/hooks/use-control";
import { useNodeId } from "@/hooks/use-node";
import { useCan } from "@/hooks/use-session";
import { formatDuration } from "@/lib/utils";

type Payload = Record<string, unknown>;

export default function RuntimePage() {
  const nodeId = useNodeId();
  const canReload = useCan("admin");
  const info = useControl<Payload>("/v1/system/info", { refetchInterval: 15_000 });
  const gates = useControl<Payload>("/v1/runtime/gates", { refetchInterval: 10_000 });
  const initialization = useControl<Payload>("/v1/runtime/initialization", {
    refetchInterval: 15_000,
  });
  const limits = useControl<Payload>("/v1/limits/effective", { refetchInterval: 60_000 });
  const [reloadOpen, setReloadOpen] = React.useState(false);
  const [reloadResult, setReloadResult] = React.useState<string | null>(null);

  return (
    <div className="flex flex-col gap-5">
      <PageSection
        actions={
          <>
            <RefreshButton
              onClick={() => {
                void info.refetch();
                void gates.refetch();
                void initialization.refetch();
              }}
              busy={info.isFetching}
            />
            {canReload ? (
              <Button size="sm" onClick={() => setReloadOpen(true)}>
                <RotateCw />
                Reload runtime
              </Button>
            ) : null}
          </>
        }
      >
        {reloadResult ? (
          <Notice title="Reload submitted">{reloadResult}</Notice>
        ) : null}
        <QueryState isLoading={info.isLoading} error={info.error}>
          <StatGrid>
            <StatCell>
              <Stat
                label="Version"
                value={String(info.data?.version ?? "—")}
                hint={String(info.data?.build_profile ?? "")}
              />
            </StatCell>
            <StatCell>
              <Stat
                label="Uptime"
                value={formatDuration(numberField(info.data, "uptime_seconds"))}
              />
            </StatCell>
            <StatCell>
              <Stat
                label="Config reloads"
                value={String(numberField(info.data, "config_reload_count") ?? 0)}
              />
            </StatCell>
            <StatCell>
              <Stat
                label="Admission"
                value={
                  <span className="text-base">
                    <StatusDot
                      state={gates.data?.accepting_new_connections === false ? "danger" : "ok"}
                      label={
                        gates.data?.accepting_new_connections === false ? "closed" : "open"
                      }
                    />
                  </span>
                }
              />
            </StatCell>
          </StatGrid>
        </QueryState>
      </PageSection>

      <div className="grid gap-4 xl:grid-cols-2">
        <SectionCard title="Runtime gates">
          <QueryState isLoading={gates.isLoading} error={gates.error}>
            <AutoFields value={gates.data} columns={1} />
          </QueryState>
        </SectionCard>
        <SectionCard title="System">
          <QueryState isLoading={info.isLoading} error={info.error}>
            <AutoFields value={info.data} columns={1} />
          </QueryState>
        </SectionCard>
      </div>

      <SectionCard
        title="Startup timeline"
        description="Per-component initialization as recorded by the startup tracker."
        bodyClassName="px-0 pb-0"
      >
        <QueryState isLoading={initialization.isLoading} error={initialization.error}>
          <div className="px-5 pb-4">
            <AutoFields
              value={initialization.data}
              columns={3}
              exclude={["components", "timeline"]}
            />
          </div>
          <AutoTable
            rows={initialization.data?.components ?? firstArray(initialization.data)}
            emptyTitle="No timeline rows"
            highlight={(row) => {
              const status = String(row.status ?? row.state ?? "").toLowerCase();
              if (status.includes("fail") || status.includes("error")) return "danger";
              if (status.includes("skip")) return "idle";
              if (status.includes("complete") || status.includes("ok")) return "ok";
              return "warn";
            }}
          />
        </QueryState>
      </SectionCard>

      <SectionCard
        title="Effective limits"
        description="Timeouts, upstream, Middle-End, and TCP policy after defaults are resolved."
      >
        <QueryState isLoading={limits.isLoading} error={limits.error}>
          <AutoFields value={limits.data} columns={3} />
          <div className="mt-4">
            <RawJson value={limits.data} />
          </div>
        </QueryState>
      </SectionCard>

      <ReloadDialog
        open={reloadOpen}
        onOpenChange={setReloadOpen}
        nodeId={nodeId}
        onSubmitted={(message) => {
          setReloadResult(message);
          setReloadOpen(false);
        }}
      />
    </div>
  );
}

function ReloadDialog({
  open,
  onOpenChange,
  nodeId,
  onSubmitted,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  nodeId: string;
  onSubmitted: (message: string) => void;
}) {
  const [policy, setPolicy] = React.useState("rollback");
  const [error, setError] = React.useState<string | null>(null);
  const [busy, setBusy] = React.useState(false);
  const [status, setStatus] = React.useState<Payload | null>(null);

  async function submit() {
    setBusy(true);
    setError(null);
    try {
      const accepted = await controlData<{ reload_id: number }>(nodeId, "/v1/system/reload", {
        method: "POST",
        body: { failure_policy: policy },
      });
      onSubmitted(`Reload ${accepted.reload_id} accepted with failure policy "${policy}".`);
      // A reload is asynchronous; the first status read tells the operator
      // whether it reached the activation barrier at all.
      const result = await controlData<Payload>(nodeId, `/v1/system/reload/${accepted.reload_id}`);
      setStatus(result);
    } catch (failure) {
      setError(failure instanceof Error ? failure.message : "Reload was refused");
    } finally {
      setBusy(false);
    }
  }

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>Reload runtime</DialogTitle>
          <DialogDescription>
            Builds a new runtime generation from the configuration on disk and swaps it in. Existing
            sessions drain rather than break.
          </DialogDescription>
        </DialogHeader>
        <DialogBody>
          <div className="flex flex-col gap-1.5">
            <Label>Failure policy</Label>
            <Select value={policy} onValueChange={setPolicy}>
              <SelectTrigger>
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                <SelectItem value="rollback">rollback — restore the previous config</SelectItem>
                <SelectItem value="keep">keep — leave the new config on disk</SelectItem>
              </SelectContent>
            </Select>
          </div>
          {status ? <RawJson value={status} label="Reload status" /> : null}
          {error ? <Notice tone="danger">{error}</Notice> : null}
          <Notice tone="warn">
            Process-owned fields — listeners, the API address, logging — need a restart and are not
            applied by a reload.
          </Notice>
        </DialogBody>
        <DialogFooter>
          <Button variant="ghost" onClick={() => onOpenChange(false)}>
            Cancel
          </Button>
          <Button onClick={() => void submit()} disabled={busy}>
            <Play />
            {busy ? "Submitting…" : "Reload"}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

function numberField(payload: Payload | undefined, key: string): number | undefined {
  const value = payload?.[key];
  return typeof value === "number" ? value : undefined;
}

function firstArray(payload: Payload | undefined): unknown[] {
  if (!payload) return [];
  for (const value of Object.values(payload)) {
    if (Array.isArray(value) && value.length > 0 && typeof value[0] === "object") return value;
  }
  return [];
}
