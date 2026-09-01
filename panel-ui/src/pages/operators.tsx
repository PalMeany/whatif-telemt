import * as React from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Ellipsis, Plus, Trash2 } from "lucide-react";
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
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { Notice, StatusDot } from "@/components/ui/feedback";
import { PageSection, QueryState, RefreshButton, SectionCard } from "@/components/page";
import { panelApi } from "@/lib/api";
import { useSession } from "@/hooks/use-session";
import { formatRelative, formatTime } from "@/lib/utils";
import type { OperatorView, Role } from "@/lib/types";

const ROLE_HELP: Record<Role, string> = {
  viewer: "Reads every view; changes nothing.",
  operator: "Reads every view and manages proxy users and quotas.",
  admin: "Everything, including configuration, node links, and panel accounts.",
};

export default function OperatorsPage() {
  const { session } = useSession();
  const queryClient = useQueryClient();
  const [creating, setCreating] = React.useState(false);
  const [error, setError] = React.useState<string | null>(null);

  const operators = useQuery({
    queryKey: ["operators"],
    queryFn: () => panelApi<{ operators: OperatorView[] }>("/operators"),
    retry: false,
  });

  const invalidate = () => void queryClient.invalidateQueries({ queryKey: ["operators"] });

  const patch = useMutation({
    mutationFn: (input: { id: string; body: Record<string, unknown> }) =>
      panelApi(`/operators/${encodeURIComponent(input.id)}`, {
        method: "PATCH",
        body: input.body,
      }),
    onSuccess: () => {
      setError(null);
      invalidate();
    },
    onError: (failure: Error) => setError(failure.message),
  });

  const remove = useMutation({
    mutationFn: (id: string) =>
      panelApi(`/operators/${encodeURIComponent(id)}`, { method: "DELETE" }),
    onSuccess: () => {
      setError(null);
      invalidate();
    },
    onError: (failure: Error) => setError(failure.message),
  });

  return (
    <div className="flex flex-col gap-5">
      <PageSection
        description="Panel accounts. Changing a role, password, or disabled flag ends that account's sessions."
        actions={
          <>
            <RefreshButton onClick={() => void operators.refetch()} busy={operators.isFetching} />
            <Button size="sm" onClick={() => setCreating(true)}>
              <Plus />
              New operator
            </Button>
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
            isLoading={operators.isLoading}
            error={operators.error}
            isEmpty={(operators.data?.operators.length ?? 0) === 0}
            emptyTitle="No operators"
          >
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead>Operator</TableHead>
                  <TableHead>Role</TableHead>
                  <TableHead>Second factor</TableHead>
                  <TableHead className="text-right">Sessions</TableHead>
                  <TableHead>Last login</TableHead>
                  <TableHead className="w-10" />
                </TableRow>
              </TableHeader>
              <TableBody>
                {(operators.data?.operators ?? []).map((operator) => (
                  <TableRow key={operator.id}>
                    <TableCell>
                      <div className="flex items-center gap-2">
                        <StatusDot state={operator.disabled ? "idle" : "ok"} />
                        <span className="font-medium">{operator.username}</span>
                        {operator.id === session?.operator_id ? (
                          <Badge variant="outline">you</Badge>
                        ) : null}
                        {operator.must_change_password ? (
                          <Badge variant="warn">must change password</Badge>
                        ) : null}
                      </div>
                      <div className="text-[11px] text-muted-foreground">
                        created {formatTime(operator.created_at)}
                      </div>
                    </TableCell>
                    <TableCell>
                      <Badge variant={operator.role === "admin" ? "solid" : "default"}>
                        {operator.role}
                      </Badge>
                    </TableCell>
                    <TableCell>
                      {operator.totp_enabled ? (
                        <Badge variant="ok">enrolled</Badge>
                      ) : (
                        <span className="text-[13px] text-muted-foreground">none</span>
                      )}
                    </TableCell>
                    <TableCell className="tabular text-right">{operator.active_sessions}</TableCell>
                    <TableCell className="text-[13px] text-muted-foreground">
                      {formatRelative(operator.last_login_at ?? undefined)}
                    </TableCell>
                    <TableCell className="text-right">
                      <DropdownMenu>
                        <DropdownMenuTrigger asChild>
                          <Button
                            variant="ghost"
                            size="icon-sm"
                            aria-label={`Actions for ${operator.username}`}
                          >
                            <Ellipsis />
                          </Button>
                        </DropdownMenuTrigger>
                        <DropdownMenuContent align="end">
                          {(["viewer", "operator", "admin"] as Role[])
                            .filter((role) => role !== operator.role)
                            .map((role) => (
                              <DropdownMenuItem
                                key={role}
                                onSelect={() =>
                                  patch.mutate({ id: operator.id, body: { role } })
                                }
                              >
                                Set role: {role}
                              </DropdownMenuItem>
                            ))}
                          <DropdownMenuSeparator />
                          <DropdownMenuItem
                            onSelect={() =>
                              patch.mutate({
                                id: operator.id,
                                body: { disabled: !operator.disabled },
                              })
                            }
                          >
                            {operator.disabled ? "Enable" : "Disable"}
                          </DropdownMenuItem>
                          {operator.totp_enabled ? (
                            <DropdownMenuItem
                              onSelect={() =>
                                patch.mutate({ id: operator.id, body: { reset_totp: true } })
                              }
                            >
                              Reset second factor
                            </DropdownMenuItem>
                          ) : null}
                          <DropdownMenuItem
                            onSelect={() => {
                              const password = window.prompt(
                                `New password for "${operator.username}"`,
                              );
                              if (password) {
                                patch.mutate({ id: operator.id, body: { password } });
                              }
                            }}
                          >
                            Set password
                          </DropdownMenuItem>
                          {operator.id !== session?.operator_id ? (
                            <>
                              <DropdownMenuSeparator />
                              <DropdownMenuItem
                                destructive
                                onSelect={() => {
                                  if (window.confirm(`Delete operator "${operator.username}"?`)) {
                                    remove.mutate(operator.id);
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

        <SectionCard title="Roles">
          <dl className="flex flex-col gap-2">
            {(Object.keys(ROLE_HELP) as Role[]).map((role) => (
              <div key={role} className="flex gap-3 text-[13px]">
                <dt className="w-20 shrink-0 font-medium">{role}</dt>
                <dd className="text-muted-foreground">{ROLE_HELP[role]}</dd>
              </div>
            ))}
          </dl>
        </SectionCard>
      </PageSection>

      <CreateOperatorDialog
        open={creating}
        onOpenChange={setCreating}
        onCreated={() => {
          setCreating(false);
          invalidate();
        }}
      />
    </div>
  );
}

function CreateOperatorDialog({
  open,
  onOpenChange,
  onCreated,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  onCreated: () => void;
}) {
  const [username, setUsername] = React.useState("");
  const [password, setPassword] = React.useState("");
  const [role, setRole] = React.useState<Role>("viewer");
  const [error, setError] = React.useState<string | null>(null);
  const [busy, setBusy] = React.useState(false);

  React.useEffect(() => {
    if (!open) return;
    setUsername("");
    setPassword("");
    setRole("viewer");
    setError(null);
  }, [open]);

  async function submit(event: React.FormEvent) {
    event.preventDefault();
    setBusy(true);
    setError(null);
    try {
      await panelApi("/operators", {
        method: "POST",
        body: { username, password, role, must_change_password: true },
      });
      onCreated();
    } catch (failure) {
      setError(failure instanceof Error ? failure.message : "Could not create the operator");
    } finally {
      setBusy(false);
    }
  }

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent>
        <form onSubmit={submit}>
          <DialogHeader>
            <DialogTitle>New operator</DialogTitle>
            <DialogDescription>
              The account starts with a provisional password it must replace at first sign-in.
            </DialogDescription>
          </DialogHeader>
          <DialogBody>
            <div className="flex flex-col gap-1.5">
              <Label htmlFor="operator-username">Username</Label>
              <Input
                id="operator-username"
                value={username}
                required
                autoFocus
                onChange={(event) => setUsername(event.target.value)}
              />
            </div>
            <div className="flex flex-col gap-1.5">
              <Label htmlFor="operator-password">Initial password</Label>
              <Input
                id="operator-password"
                value={password}
                required
                onChange={(event) => setPassword(event.target.value)}
              />
            </div>
            <div className="flex flex-col gap-1.5">
              <Label>Role</Label>
              <Select value={role} onValueChange={(value) => setRole(value as Role)}>
                <SelectTrigger>
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  <SelectItem value="viewer">viewer</SelectItem>
                  <SelectItem value="operator">operator</SelectItem>
                  <SelectItem value="admin">admin</SelectItem>
                </SelectContent>
              </Select>
              <p className="text-[12px] text-muted-foreground">{ROLE_HELP[role]}</p>
            </div>
            {error ? <Notice tone="danger">{error}</Notice> : null}
          </DialogBody>
          <DialogFooter>
            <Button type="button" variant="ghost" onClick={() => onOpenChange(false)}>
              Cancel
            </Button>
            <Button type="submit" disabled={busy}>
              {busy ? "Creating…" : "Create operator"}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}
