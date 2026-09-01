import * as React from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Label } from "@/components/ui/label";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import { Badge } from "@/components/ui/badge";
import { Notice } from "@/components/ui/feedback";
import { Field } from "@/components/ui/stat";
import { PageSection, QueryState, SectionCard } from "@/components/page";
import { panelApi } from "@/lib/api";
import { useNodes } from "@/hooks/use-node";
import { useSession } from "@/hooks/use-session";
import { useTheme } from "@/hooks/use-theme";
import { formatDuration } from "@/lib/utils";

type SettingsView = {
  default_node_id: string | null;
  appearance: string;
  limits: Record<string, number | boolean>;
  cluster: { enabled: boolean; role: string; advertise_url: string; poll_interval_secs: number };
};

export default function SettingsPage() {
  const { session, bootstrap } = useSession();
  const { nodes } = useNodes();
  const queryClient = useQueryClient();
  const { theme, setTheme } = useTheme();
  const isAdmin = session?.role === "admin";
  const [error, setError] = React.useState<string | null>(null);

  const settings = useQuery({
    queryKey: ["settings"],
    queryFn: () => panelApi<SettingsView>("/settings"),
    retry: false,
  });

  const patch = useMutation({
    mutationFn: (body: Record<string, unknown>) =>
      panelApi("/settings", { method: "PATCH", body }),
    onSuccess: () => {
      setError(null);
      void queryClient.invalidateQueries({ queryKey: ["settings"] });
    },
    onError: (failure: Error) => setError(failure.message),
  });

  return (
    <div className="flex flex-col gap-5">
      <PageSection description="Panel preferences and the bounds this panel was configured with.">
        {error ? (
          <Notice tone="danger" title="Could not save">
            {error}
          </Notice>
        ) : null}
        <QueryState isLoading={settings.isLoading} error={settings.error}>
          <div className="grid gap-4 xl:grid-cols-2">
            <SectionCard title="Preferences">
              <div className="flex flex-col gap-4">
                <div className="flex flex-col gap-1.5">
                  <Label>Theme</Label>
                  <Select
                    value={theme}
                    onValueChange={(value) => setTheme(value as "dark" | "light")}
                  >
                    <SelectTrigger className="max-w-xs">
                      <SelectValue />
                    </SelectTrigger>
                    <SelectContent>
                      <SelectItem value="dark">Dark</SelectItem>
                      <SelectItem value="light">Light</SelectItem>
                    </SelectContent>
                  </Select>
                </div>
                <div className="flex flex-col gap-1.5">
                  <Label>Default node</Label>
                  <Select
                    value={settings.data?.default_node_id ?? "local"}
                    disabled={!isAdmin}
                    onValueChange={(value) => patch.mutate({ default_node_id: value })}
                  >
                    <SelectTrigger className="max-w-xs">
                      <SelectValue />
                    </SelectTrigger>
                    <SelectContent>
                      {nodes.map((node) => (
                        <SelectItem key={node.id} value={node.id}>
                          {node.name}
                        </SelectItem>
                      ))}
                    </SelectContent>
                  </Select>
                  <p className="text-[12px] text-muted-foreground">
                    {isAdmin
                      ? "Applies to every operator who has not chosen a node in this browser."
                      : "Only an administrator can change the fleet default."}
                  </p>
                </div>
              </div>
            </SectionCard>

            <SectionCard
              title="Session and credential policy"
              description="Configured under [panel]; changing it needs a restart."
            >
              <Field
                label="Session lifetime"
                value={formatDuration(Number(settings.data?.limits.session_ttl_secs))}
              />
              <Field
                label="Idle timeout"
                value={formatDuration(Number(settings.data?.limits.session_idle_timeout_secs))}
              />
              <Field
                label="Sessions per operator"
                value={String(settings.data?.limits.max_sessions_per_operator ?? "—")}
                mono
              />
              <Field
                label="Login attempts before lockout"
                value={String(settings.data?.limits.login_max_attempts ?? "—")}
                mono
              />
              <Field
                label="Lockout"
                value={formatDuration(Number(settings.data?.limits.login_lockout_secs))}
              />
              <Field
                label="Minimum password length"
                value={String(settings.data?.limits.password_min_length ?? "—")}
                mono
              />
              <Field
                label="Second factor required"
                value={settings.data?.limits.require_totp ? "yes" : "no"}
              />
              <Field
                label="Audit retention"
                value={`${settings.data?.limits.audit_retention_days ?? "—"} days`}
              />
            </SectionCard>

            <SectionCard title="Federation" description="How this node participates in a fleet.">
              <Field
                label="Enabled"
                value={
                  <Badge variant={settings.data?.cluster.enabled ? "ok" : "outline"}>
                    {settings.data?.cluster.enabled ? "yes" : "no"}
                  </Badge>
                }
              />
              <Field label="Role" value={settings.data?.cluster.role ?? "—"} />
              <Field
                label="Advertise URL"
                value={settings.data?.cluster.advertise_url || "not set"}
                mono
              />
              <Field
                label="Health poll"
                value={formatDuration(settings.data?.cluster.poll_interval_secs)}
              />
              <Field label="Linked nodes" value={String(bootstrap?.node.linked_nodes ?? 0)} mono />
            </SectionCard>

            <SectionCard title="This build">
              <Field label="telemt version" value={bootstrap?.version ?? "—"} mono />
              <Field label="Node id" value={bootstrap?.node.id ?? "—"} mono />
              <Field label="Node name" value={bootstrap?.node.name ?? "—"} />
              <Field
                label="UI bundle"
                value={bootstrap?.bundled_ui ? "embedded" : "not embedded"}
              />
              <Field label="Audit log" value={bootstrap?.audit_enabled ? "enabled" : "disabled"} />
            </SectionCard>
          </div>
        </QueryState>
      </PageSection>
    </div>
  );
}
