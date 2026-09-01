import * as React from "react";
import { useMutation, useQueryClient } from "@tanstack/react-query";
import {
  Ellipsis,
  KeyRound,
  Link2,
  Plus,
  RotateCw,
  Search,
  Trash2,
} from "lucide-react";
import { Badge } from "@/components/ui/badge";
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
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Switch } from "@/components/ui/switch";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import { CodeBlock, CopyButton, Notice, StatusDot } from "@/components/ui/feedback";
import { MeterBar } from "@/components/ui/sparkline";
import { Field } from "@/components/ui/stat";
import { PageSection, QueryState, RefreshButton, SectionCard } from "@/components/page";
import { controlData } from "@/lib/api";
import { useControl } from "@/hooks/use-control";
import { useNodeId } from "@/hooks/use-node";
import { useCan } from "@/hooks/use-session";
import { formatBytes, formatCount } from "@/lib/utils";
import type { UserInfo } from "@/lib/types";

type CreateResponse = { user: UserInfo; secret: string };

export default function UsersPage() {
  const nodeId = useNodeId();
  const queryClient = useQueryClient();
  const canManage = useCan("operator");
  const users = useControl<UserInfo[]>("/v1/users", { refetchInterval: 15_000 });
  const [filter, setFilter] = React.useState("");
  const [creating, setCreating] = React.useState(false);
  const [editing, setEditing] = React.useState<UserInfo | null>(null);
  const [links, setLinks] = React.useState<UserInfo | null>(null);
  const [secret, setSecret] = React.useState<{ username: string; secret: string } | null>(null);
  const [error, setError] = React.useState<string | null>(null);

  const invalidate = React.useCallback(() => {
    void queryClient.invalidateQueries({ queryKey: ["control", nodeId] });
  }, [queryClient, nodeId]);

  const action = useMutation({
    mutationFn: async (input: { path: string; method: string; body?: unknown }) =>
      controlData<unknown>(nodeId, input.path, { method: input.method, body: input.body }),
    onSuccess: () => {
      setError(null);
      invalidate();
    },
    onError: (failure: Error) => setError(failure.message),
  });

  const rotate = useMutation({
    mutationFn: async (username: string) =>
      controlData<CreateResponse>(nodeId, `/v1/users/${encodeURIComponent(username)}/rotate-secret`, {
        method: "POST",
      }),
    onSuccess: (result) => {
      setSecret({ username: result.user.username, secret: result.secret });
      invalidate();
    },
    onError: (failure: Error) => setError(failure.message),
  });

  const rows = React.useMemo(() => {
    const list = users.data ?? [];
    const needle = filter.trim().toLowerCase();
    if (!needle) return list;
    return list.filter((user) => user.username.toLowerCase().includes(needle));
  }, [users.data, filter]);

  return (
    <div className="flex flex-col gap-5">
      <PageSection
        actions={
          <>
            <div className="relative">
              <Search className="pointer-events-none absolute left-2.5 top-1/2 size-3.5 -translate-y-1/2 text-muted-foreground" />
              <Input
                value={filter}
                onChange={(event) => setFilter(event.target.value)}
                placeholder="Filter users"
                className="h-9 w-52 pl-8"
                aria-label="Filter users"
              />
            </div>
            <RefreshButton onClick={() => void users.refetch()} busy={users.isFetching} />
            {canManage ? (
              <Button size="sm" onClick={() => setCreating(true)}>
                <Plus />
                New user
              </Button>
            ) : null}
          </>
        }
      >
        {error ? (
          <Notice tone="danger" title="Action failed">
            {error}
          </Notice>
        ) : null}
        <SectionCard bodyClassName="px-0 pb-0">
          <QueryState
            isLoading={users.isLoading}
            error={users.error}
            isEmpty={rows.length === 0}
            emptyTitle={filter ? "No user matches that filter" : "No users configured"}
            emptyDescription={
              filter ? undefined : "Users live in [access.users] and can be created here."
            }
            skeletonRows={5}
          >
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead>User</TableHead>
                  <TableHead className="text-right">Conns</TableHead>
                  <TableHead className="text-right">Unique IPs</TableHead>
                  <TableHead className="w-52">Quota</TableHead>
                  <TableHead className="text-right">Traffic</TableHead>
                  <TableHead className="w-10" />
                </TableRow>
              </TableHeader>
              <TableBody>
                {rows.map((user) => (
                  <TableRow key={user.username}>
                    <TableCell>
                      <div className="flex items-center gap-2">
                        <StatusDot state={user.enabled ? "ok" : "idle"} />
                        <span className="font-medium">{user.username}</span>
                        {!user.in_runtime ? (
                          <Badge variant="warn" title="Written to disk, not yet in the runtime">
                            pending reload
                          </Badge>
                        ) : null}
                        {user.expiration_rfc3339 ? (
                          <Badge variant="outline">expires</Badge>
                        ) : null}
                      </div>
                    </TableCell>
                    <TableCell className="tabular text-right">
                      {formatCount(user.current_connections)}
                      {user.max_tcp_conns ? (
                        <span className="text-muted-foreground">/{user.max_tcp_conns}</span>
                      ) : null}
                    </TableCell>
                    <TableCell className="tabular text-right">
                      {formatCount(user.active_unique_ips)}
                      {user.max_unique_ips ? (
                        <span className="text-muted-foreground">/{user.max_unique_ips}</span>
                      ) : null}
                    </TableCell>
                    <TableCell>
                      {user.data_quota_bytes ? (
                        <div className="flex flex-col gap-1">
                          <MeterBar value={user.total_octets} total={user.data_quota_bytes} />
                          <span className="tabular text-[11px] text-muted-foreground">
                            {formatBytes(user.total_octets)} / {formatBytes(user.data_quota_bytes)}
                          </span>
                        </div>
                      ) : (
                        <span className="text-[12px] text-muted-foreground">unlimited</span>
                      )}
                    </TableCell>
                    <TableCell className="tabular text-right">
                      {formatBytes(user.total_octets)}
                    </TableCell>
                    <TableCell className="text-right">
                      <DropdownMenu>
                        <DropdownMenuTrigger asChild>
                          <Button variant="ghost" size="icon-sm" aria-label={`Actions for ${user.username}`}>
                            <Ellipsis />
                          </Button>
                        </DropdownMenuTrigger>
                        <DropdownMenuContent align="end">
                          <DropdownMenuItem onSelect={() => setLinks(user)}>
                            <Link2 />
                            Connection links
                          </DropdownMenuItem>
                          {canManage ? (
                            <>
                              <DropdownMenuItem onSelect={() => setEditing(user)}>
                                Edit limits
                              </DropdownMenuItem>
                              <DropdownMenuItem
                                onSelect={() =>
                                  action.mutate({
                                    path: `/v1/users/${encodeURIComponent(user.username)}/${
                                      user.enabled ? "disable" : "enable"
                                    }`,
                                    method: "POST",
                                  })
                                }
                              >
                                {user.enabled ? "Disable" : "Enable"}
                              </DropdownMenuItem>
                              <DropdownMenuItem onSelect={() => rotate.mutate(user.username)}>
                                <KeyRound />
                                Rotate secret
                              </DropdownMenuItem>
                              <DropdownMenuItem
                                onSelect={() =>
                                  action.mutate({
                                    path: `/v1/users/${encodeURIComponent(user.username)}/reset-quota`,
                                    method: "POST",
                                  })
                                }
                              >
                                <RotateCw />
                                Reset quota
                              </DropdownMenuItem>
                              <DropdownMenuSeparator />
                              <DropdownMenuItem
                                destructive
                                onSelect={() => {
                                  if (
                                    window.confirm(
                                      `Delete user "${user.username}"? Active sessions are cancelled.`,
                                    )
                                  ) {
                                    action.mutate({
                                      path: `/v1/users/${encodeURIComponent(user.username)}`,
                                      method: "DELETE",
                                    });
                                  }
                                }}
                              >
                                <Trash2 />
                                Delete
                              </DropdownMenuItem>
                            </>
                          ) : null}
                        </DropdownMenuContent>
                      </DropdownMenu>
                    </TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          </QueryState>
        </SectionCard>
      </PageSection>

      <CreateUserDialog
        open={creating}
        onOpenChange={setCreating}
        nodeId={nodeId}
        onCreated={(result) => {
          setSecret({ username: result.user.username, secret: result.secret });
          invalidate();
        }}
      />
      <EditUserDialog
        user={editing}
        onOpenChange={(open) => !open && setEditing(null)}
        nodeId={nodeId}
        onSaved={() => {
          setEditing(null);
          invalidate();
        }}
      />
      <LinksDialog user={links} onOpenChange={(open) => !open && setLinks(null)} />
      <SecretDialog value={secret} onOpenChange={(open) => !open && setSecret(null)} />
    </div>
  );
}

const NUMERIC_FIELDS = [
  { key: "max_tcp_conns", label: "Max TCP connections" },
  { key: "max_unique_ips", label: "Max unique IPs" },
  { key: "data_quota_bytes", label: "Data quota (bytes)" },
  { key: "rate_limit_up_bps", label: "Uplink limit (bps)" },
  { key: "rate_limit_down_bps", label: "Downlink limit (bps)" },
] as const;

function CreateUserDialog({
  open,
  onOpenChange,
  nodeId,
  onCreated,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  nodeId: string;
  onCreated: (result: CreateResponse) => void;
}) {
  const [username, setUsername] = React.useState("");
  const [secret, setSecret] = React.useState("");
  const [adTag, setAdTag] = React.useState("");
  const [numbers, setNumbers] = React.useState<Record<string, string>>({});
  const [enabled, setEnabled] = React.useState(true);
  const [error, setError] = React.useState<string | null>(null);
  const [busy, setBusy] = React.useState(false);

  React.useEffect(() => {
    if (!open) return;
    setUsername("");
    setSecret("");
    setAdTag("");
    setNumbers({});
    setEnabled(true);
    setError(null);
  }, [open]);

  async function submit(event: React.FormEvent) {
    event.preventDefault();
    setBusy(true);
    setError(null);
    try {
      const body: Record<string, unknown> = { username, enabled };
      if (secret.trim()) body.secret = secret.trim();
      if (adTag.trim()) body.user_ad_tag = adTag.trim();
      for (const field of NUMERIC_FIELDS) {
        const raw = numbers[field.key];
        if (raw && raw.trim()) body[field.key] = Number(raw);
      }
      const result = await controlData<CreateResponse>(nodeId, "/v1/users", {
        method: "POST",
        body,
      });
      onCreated(result);
      onOpenChange(false);
    } catch (failure) {
      setError(failure instanceof Error ? failure.message : "Could not create the user");
    } finally {
      setBusy(false);
    }
  }

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent>
        <form onSubmit={submit}>
          <DialogHeader>
            <DialogTitle>New user</DialogTitle>
            <DialogDescription>
              The secret is generated when left empty and shown once after creation.
            </DialogDescription>
          </DialogHeader>
          <DialogBody>
            <div className="flex flex-col gap-1.5">
              <Label htmlFor="new-username">Username</Label>
              <Input
                id="new-username"
                value={username}
                required
                autoFocus
                onChange={(event) => setUsername(event.target.value)}
              />
            </div>
            <div className="flex flex-col gap-1.5">
              <Label htmlFor="new-secret">Secret (32 hex characters, optional)</Label>
              <Input
                id="new-secret"
                value={secret}
                className="font-mono"
                placeholder="generated when empty"
                onChange={(event) => setSecret(event.target.value)}
              />
            </div>
            <div className="flex flex-col gap-1.5">
              <Label htmlFor="new-adtag">Ad tag (optional)</Label>
              <Input
                id="new-adtag"
                value={adTag}
                className="font-mono"
                onChange={(event) => setAdTag(event.target.value)}
              />
            </div>
            <div className="grid gap-3 sm:grid-cols-2">
              {NUMERIC_FIELDS.map((field) => (
                <div key={field.key} className="flex flex-col gap-1.5">
                  <Label htmlFor={`new-${field.key}`}>{field.label}</Label>
                  <Input
                    id={`new-${field.key}`}
                    inputMode="numeric"
                    value={numbers[field.key] ?? ""}
                    placeholder="unlimited"
                    onChange={(event) =>
                      setNumbers((previous) => ({
                        ...previous,
                        [field.key]: event.target.value.replace(/\D/g, ""),
                      }))
                    }
                  />
                </div>
              ))}
            </div>
            <div className="flex items-center justify-between rounded-md border border-[var(--border)] px-3 py-2.5">
              <Label htmlFor="new-enabled">Enabled immediately</Label>
              <Switch id="new-enabled" checked={enabled} onCheckedChange={setEnabled} />
            </div>
            {error ? <Notice tone="danger">{error}</Notice> : null}
          </DialogBody>
          <DialogFooter>
            <Button type="button" variant="ghost" onClick={() => onOpenChange(false)}>
              Cancel
            </Button>
            <Button type="submit" disabled={busy}>
              {busy ? "Creating…" : "Create user"}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}

function EditUserDialog({
  user,
  onOpenChange,
  nodeId,
  onSaved,
}: {
  user: UserInfo | null;
  onOpenChange: (open: boolean) => void;
  nodeId: string;
  onSaved: () => void;
}) {
  const [numbers, setNumbers] = React.useState<Record<string, string>>({});
  const [adTag, setAdTag] = React.useState("");
  const [expiration, setExpiration] = React.useState("");
  const [error, setError] = React.useState<string | null>(null);
  const [busy, setBusy] = React.useState(false);

  React.useEffect(() => {
    if (!user) return;
    setNumbers({
      max_tcp_conns: user.max_tcp_conns?.toString() ?? "",
      max_unique_ips: user.max_unique_ips?.toString() ?? "",
      data_quota_bytes: user.data_quota_bytes?.toString() ?? "",
      rate_limit_up_bps: user.rate_limit_up_bps?.toString() ?? "",
      rate_limit_down_bps: user.rate_limit_down_bps?.toString() ?? "",
    });
    setAdTag(user.user_ad_tag ?? "");
    setExpiration(user.expiration_rfc3339 ?? "");
    setError(null);
  }, [user]);

  async function submit(event: React.FormEvent) {
    event.preventDefault();
    if (!user) return;
    setBusy(true);
    setError(null);
    try {
      // The Control API treats an explicit null as "remove this field", which
      // is exactly what an emptied input means here.
      const body: Record<string, unknown> = {
        user_ad_tag: adTag.trim() ? adTag.trim() : null,
        expiration_rfc3339: expiration.trim() ? expiration.trim() : null,
      };
      for (const field of NUMERIC_FIELDS) {
        const raw = numbers[field.key];
        body[field.key] = raw && raw.trim() ? Number(raw) : null;
      }
      await controlData(nodeId, `/v1/users/${encodeURIComponent(user.username)}`, {
        method: "PATCH",
        body,
      });
      onSaved();
    } catch (failure) {
      setError(failure instanceof Error ? failure.message : "Could not save the user");
    } finally {
      setBusy(false);
    }
  }

  return (
    <Dialog open={Boolean(user)} onOpenChange={onOpenChange}>
      <DialogContent>
        <form onSubmit={submit}>
          <DialogHeader>
            <DialogTitle>{user?.username}</DialogTitle>
            <DialogDescription>
              An empty field removes the limit. Changes are written to the configuration file.
            </DialogDescription>
          </DialogHeader>
          <DialogBody>
            <div className="grid gap-3 sm:grid-cols-2">
              {NUMERIC_FIELDS.map((field) => (
                <div key={field.key} className="flex flex-col gap-1.5">
                  <Label htmlFor={`edit-${field.key}`}>{field.label}</Label>
                  <Input
                    id={`edit-${field.key}`}
                    inputMode="numeric"
                    value={numbers[field.key] ?? ""}
                    placeholder="unlimited"
                    onChange={(event) =>
                      setNumbers((previous) => ({
                        ...previous,
                        [field.key]: event.target.value.replace(/\D/g, ""),
                      }))
                    }
                  />
                </div>
              ))}
            </div>
            <div className="flex flex-col gap-1.5">
              <Label htmlFor="edit-adtag">Ad tag</Label>
              <Input
                id="edit-adtag"
                value={adTag}
                className="font-mono"
                onChange={(event) => setAdTag(event.target.value)}
              />
            </div>
            <div className="flex flex-col gap-1.5">
              <Label htmlFor="edit-expiration">Expiration (RFC 3339)</Label>
              <Input
                id="edit-expiration"
                value={expiration}
                className="font-mono"
                placeholder="2026-12-31T23:59:59Z"
                onChange={(event) => setExpiration(event.target.value)}
              />
            </div>
            {error ? <Notice tone="danger">{error}</Notice> : null}
          </DialogBody>
          <DialogFooter>
            <Button type="button" variant="ghost" onClick={() => onOpenChange(false)}>
              Cancel
            </Button>
            <Button type="submit" disabled={busy}>
              {busy ? "Saving…" : "Save"}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}

function LinksDialog({
  user,
  onOpenChange,
}: {
  user: UserInfo | null;
  onOpenChange: (open: boolean) => void;
}) {
  const entries = React.useMemo(() => collectLinks(user), [user]);
  return (
    <Dialog open={Boolean(user)} onOpenChange={onOpenChange}>
      <DialogContent wide>
        <DialogHeader>
          <DialogTitle>{user?.username} — connection links</DialogTitle>
          <DialogDescription>
            Rendered by the node from its own public host, port, and TLS domains.
          </DialogDescription>
        </DialogHeader>
        <DialogBody>
          {entries.length === 0 ? (
            <Notice>
              This node renders no links for the user. Check <code>[general.links]</code>.
            </Notice>
          ) : (
            entries.map((entry) => (
              <div key={entry.label} className="flex flex-col gap-1.5">
                <div className="flex items-center justify-between gap-2">
                  <Label>{entry.label}</Label>
                  <CopyButton value={entry.value} />
                </div>
                <CodeBlock value={entry.value} />
              </div>
            ))
          )}
          <div className="rounded-md border border-[var(--border)] px-3 py-2">
            <Field label="Active IPs" value={user?.active_unique_ips_list.join(", ") || "none"} mono />
            <Field label="Recent IPs" value={user?.recent_unique_ips_list.join(", ") || "none"} mono />
          </div>
        </DialogBody>
      </DialogContent>
    </Dialog>
  );
}

function SecretDialog({
  value,
  onOpenChange,
}: {
  value: { username: string; secret: string } | null;
  onOpenChange: (open: boolean) => void;
}) {
  return (
    <Dialog open={Boolean(value)} onOpenChange={onOpenChange}>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>Secret for {value?.username}</DialogTitle>
          <DialogDescription>
            The node stores this secret but the panel will not show it again.
          </DialogDescription>
        </DialogHeader>
        <DialogBody>
          <CodeBlock value={value?.secret ?? ""} />
          <CopyButton value={value?.secret ?? ""} label="Copy secret" className="self-start" />
        </DialogBody>
        <DialogFooter>
          <Button onClick={() => onOpenChange(false)}>Done</Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

/** Flattens the Control API's link object into labelled rows. */
function collectLinks(user: UserInfo | null): { label: string; value: string }[] {
  if (!user) return [];
  const out: { label: string; value: string }[] = [];
  for (const [key, value] of Object.entries(user.links ?? {})) {
    if (typeof value === "string" && value) {
      out.push({ label: key.replace(/_/g, " "), value });
    }
    if (Array.isArray(value)) {
      for (const item of value) {
        if (typeof item === "string" && item) {
          out.push({ label: key.replace(/_/g, " "), value: item });
        }
        if (item && typeof item === "object" && "link" in item) {
          const entry = item as { domain?: string; link?: string };
          if (entry.link) out.push({ label: entry.domain ?? key, value: entry.link });
        }
      }
    }
  }
  return out;
}
