import * as React from "react";
import { Save, ShieldAlert } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Label } from "@/components/ui/label";
import { Textarea } from "@/components/ui/input";
import { Switch } from "@/components/ui/switch";
import { Badge } from "@/components/ui/badge";
import { CodeBlock, Notice } from "@/components/ui/feedback";
import { PageSection, QueryState, RefreshButton, SectionCard } from "@/components/page";
import { controlApi } from "@/lib/api";
import { useControl } from "@/hooks/use-control";
import { useNodeId } from "@/hooks/use-node";
import { useCan } from "@/hooks/use-session";
import { useQuery } from "@tanstack/react-query";

type PatchResponse = {
  revision: string;
  runtime_reload_required: boolean;
  process_restart_required: boolean;
  deferred_process_fields: string[];
  changed: string[];
  reload?: { reload_id: number };
};

export default function ConfigPage() {
  const nodeId = useNodeId();
  const canEdit = useCan("admin");
  const [draft, setDraft] = React.useState("");
  const [dirty, setDirty] = React.useState(false);
  const [applyReload, setApplyReload] = React.useState(true);
  const [error, setError] = React.useState<string | null>(null);
  const [result, setResult] = React.useState<PatchResponse | null>(null);
  const [busy, setBusy] = React.useState(false);

  const config = useQuery({
    queryKey: ["config", nodeId],
    queryFn: () => controlApi<Record<string, unknown>>(nodeId, "/v1/config"),
    staleTime: 5_000,
    retry: false,
  });
  const posture = useControl<Record<string, unknown>>("/v1/security/posture");

  React.useEffect(() => {
    if (!config.data || dirty) return;
    setDraft(JSON.stringify(config.data.data, null, 2));
  }, [config.data, dirty]);

  const readOnly = posture.data?.api_read_only === true;

  async function save() {
    setBusy(true);
    setError(null);
    setResult(null);
    let parsed: unknown;
    try {
      parsed = JSON.parse(draft);
    } catch {
      setError("The editor does not contain valid JSON.");
      setBusy(false);
      return;
    }
    try {
      const query = applyReload ? "?reload=true&failure_policy=rollback" : "";
      const response = await controlApi<PatchResponse>(nodeId, `/v1/config${query}`, {
        method: "PATCH",
        body: parsed,
        // The Control API uses If-Match for optimistic concurrency; sending the
        // revision the editor was populated from refuses a blind overwrite of
        // someone else's concurrent change.
        headers: config.data?.revision ? { "If-Match": config.data.revision } : undefined,
      });
      setResult(response.data);
      setDirty(false);
      await config.refetch();
    } catch (failure) {
      setError(failure instanceof Error ? failure.message : "The patch was refused");
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="flex flex-col gap-5">
      <PageSection
        description="Editable sections of this node's configuration file. Users live under Users, not here."
        actions={
          <>
            <RefreshButton
              onClick={() => {
                setDirty(false);
                void config.refetch();
              }}
              busy={config.isFetching}
            />
            {canEdit ? (
              <Button size="sm" onClick={() => void save()} disabled={busy || readOnly || !dirty}>
                <Save />
                {busy ? "Applying…" : "Apply"}
              </Button>
            ) : null}
          </>
        }
      >
        {readOnly ? (
          <Notice tone="warn" title="This node's Control API is read-only">
            <code>server.api.read_only</code> is set, so every mutation is refused.
          </Notice>
        ) : null}
        {!canEdit ? (
          <Notice tone="warn" title="Read-only view">
            Editing the configuration requires the admin role.
          </Notice>
        ) : null}
        {error ? (
          <Notice tone="danger" title="Patch refused">
            {error}
          </Notice>
        ) : null}
        {result ? (
          <Notice
            tone={result.process_restart_required ? "warn" : "info"}
            title={
              result.reload
                ? `Applied; reload ${result.reload.reload_id} submitted`
                : "Written to disk"
            }
          >
            <div className="flex flex-col gap-1">
              <div>
                Changed: {result.changed.length > 0 ? result.changed.join(", ") : "nothing"}
              </div>
              {result.process_restart_required ? (
                <div className="flex items-center gap-1.5">
                  <ShieldAlert className="size-3.5" />
                  Restart required for: {result.deferred_process_fields.join(", ")}
                </div>
              ) : null}
            </div>
          </Notice>
        ) : null}

        <SectionCard
          title="Managed sections"
          description={
            config.data?.revision ? `Revision ${config.data.revision.slice(0, 16)}…` : undefined
          }
          actions={
            canEdit ? (
              <label className="flex items-center gap-2 text-[12px] text-muted-foreground">
                <Switch checked={applyReload} onCheckedChange={setApplyReload} />
                Reload after write
              </label>
            ) : null
          }
        >
          <QueryState isLoading={config.isLoading} error={config.error}>
            <div className="flex flex-col gap-2">
              <Label htmlFor="config-editor">Sparse patch — only the fields you send change</Label>
              <Textarea
                id="config-editor"
                value={draft}
                spellCheck={false}
                readOnly={!canEdit || readOnly}
                rows={26}
                className="text-[12.5px]"
                onChange={(event) => {
                  setDraft(event.target.value);
                  setDirty(true);
                }}
              />
              <div className="flex flex-wrap items-center gap-2 text-[11px] text-muted-foreground">
                <Badge variant="outline">general</Badge>
                <Badge variant="outline">timeouts</Badge>
                <Badge variant="outline">censorship</Badge>
                <Badge variant="outline">upstreams</Badge>
                <Badge variant="outline">dc_overrides</Badge>
                <span>are the sections the Control API accepts.</span>
              </div>
            </div>
          </QueryState>
        </SectionCard>

        <SectionCard
          title="How a patch is applied"
          description="The same rules the Control API documents."
        >
          <ol className="flex list-decimal flex-col gap-1.5 pl-5 text-[13px] text-muted-foreground">
            <li>The body is merged into the file; untouched sections keep their comments.</li>
            <li>
              <code>If-Match</code> carries the revision this editor loaded, so a concurrent change
              is refused rather than overwritten.
            </li>
            <li>
              With reload enabled, a new runtime generation is built and swapped in; failures roll
              the file back.
            </li>
            <li>Process-owned fields are reported and need a restart.</li>
          </ol>
          <div className="mt-3">
            <CodeBlock value={'{ "censorship": { "tls_domain": "example.com" } }'} />
          </div>
        </SectionCard>
      </PageSection>
    </div>
  );
}
