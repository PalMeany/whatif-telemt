import * as React from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { KeyRound, ShieldCheck, ShieldOff } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Badge } from "@/components/ui/badge";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { CodeBlock, CopyButton, Notice } from "@/components/ui/feedback";
import { PageSection, QueryState, SectionCard } from "@/components/page";
import { panelApi } from "@/lib/api";
import { useSession } from "@/hooks/use-session";
import { formatRelative, formatTime } from "@/lib/utils";

type TotpState = { enrolled: boolean; confirmed: boolean; recovery_remaining: number };
type TotpBegin = { secret: string; uri: string };
type SessionSummary = {
  created_at: number;
  last_seen: number;
  address: string | null;
  user_agent: string;
  current: boolean;
};

export default function AccountPage() {
  const { session, refresh } = useSession();
  const queryClient = useQueryClient();
  const [message, setMessage] = React.useState<string | null>(null);
  const [error, setError] = React.useState<string | null>(null);

  const totp = useQuery({
    queryKey: ["account", "totp"],
    queryFn: () => panelApi<TotpState>("/account/totp"),
    retry: false,
  });
  const sessions = useQuery({
    queryKey: ["account", "sessions"],
    queryFn: () => panelApi<{ sessions: SessionSummary[] }>("/account/sessions"),
    retry: false,
  });

  const revokeOthers = useMutation({
    mutationFn: () => panelApi<{ revoked: number }>("/account/sessions", { method: "DELETE" }),
    onSuccess: (result) => {
      setMessage(`${result.revoked} other session${result.revoked === 1 ? "" : "s"} ended.`);
      void queryClient.invalidateQueries({ queryKey: ["account", "sessions"] });
    },
    onError: (failure: Error) => setError(failure.message),
  });

  return (
    <div className="flex flex-col gap-5">
      <PageSection description={`Signed in as ${session?.username} (${session?.role}).`}>
        {message ? <Notice title="Done">{message}</Notice> : null}
        {error ? (
          <Notice tone="danger" title="Action failed">
            {error}
          </Notice>
        ) : null}

        <div className="grid gap-4 xl:grid-cols-2">
          <ChangePasswordCard
            onDone={(text) => {
              setMessage(text);
              void queryClient.invalidateQueries({ queryKey: ["account", "sessions"] });
            }}
            onError={setError}
          />

          <SectionCard
            title="Second factor"
            description="Time-based one-time password, RFC 6238."
          >
            <QueryState isLoading={totp.isLoading} error={totp.error} skeletonRows={2}>
              <TotpSection
                state={totp.data}
                required={session?.totp_required ?? false}
                onChanged={async () => {
                  await totp.refetch();
                  await refresh();
                }}
                onError={setError}
              />
            </QueryState>
          </SectionCard>
        </div>

        <SectionCard
          title="Active sessions"
          description="Sessions live in memory only, so a restart ends all of them."
          actions={
            <Button variant="outline" size="sm" onClick={() => revokeOthers.mutate()}>
              End other sessions
            </Button>
          }
          bodyClassName="px-0 pb-0"
        >
          <QueryState
            isLoading={sessions.isLoading}
            error={sessions.error}
            isEmpty={(sessions.data?.sessions.length ?? 0) === 0}
            emptyTitle="No sessions"
          >
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead>Started</TableHead>
                  <TableHead>Last seen</TableHead>
                  <TableHead>Address</TableHead>
                  <TableHead>Client</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {(sessions.data?.sessions ?? []).map((entry) => (
                  <TableRow key={`${entry.created_at}-${entry.address ?? ""}`}>
                    <TableCell className="tabular whitespace-nowrap text-[12px]">
                      {formatTime(entry.created_at)}
                      {entry.current ? (
                        <Badge variant="ok" className="ml-2">
                          this browser
                        </Badge>
                      ) : null}
                    </TableCell>
                    <TableCell className="text-[12px] text-muted-foreground">
                      {formatRelative(entry.last_seen)}
                    </TableCell>
                    <TableCell className="font-mono text-[12px]">{entry.address ?? "—"}</TableCell>
                    <TableCell className="max-w-[26rem] truncate text-[12px] text-muted-foreground">
                      {entry.user_agent || "—"}
                    </TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          </QueryState>
        </SectionCard>
      </PageSection>
    </div>
  );
}

function ChangePasswordCard({
  onDone,
  onError,
}: {
  onDone: (message: string) => void;
  onError: (message: string) => void;
}) {
  const [current, setCurrent] = React.useState("");
  const [next, setNext] = React.useState("");
  const [confirm, setConfirm] = React.useState("");
  const [busy, setBusy] = React.useState(false);

  async function submit(event: React.FormEvent) {
    event.preventDefault();
    if (next !== confirm) {
      onError("The two new passwords do not match");
      return;
    }
    setBusy(true);
    try {
      const result = await panelApi<{ revoked_sessions: number }>("/account/password", {
        method: "POST",
        body: { current_password: current, new_password: next },
      });
      setCurrent("");
      setNext("");
      setConfirm("");
      onDone(
        `Password changed; ${result.revoked_sessions} other session${
          result.revoked_sessions === 1 ? "" : "s"
        } ended.`,
      );
    } catch (failure) {
      onError(failure instanceof Error ? failure.message : "Could not change the password");
    } finally {
      setBusy(false);
    }
  }

  return (
    <SectionCard
      title="Password"
      description="Changing it ends every other session of this account."
    >
      <form onSubmit={submit} className="flex flex-col gap-3">
        <div className="flex flex-col gap-1.5">
          <Label htmlFor="account-current">Current password</Label>
          <Input
            id="account-current"
            type="password"
            autoComplete="current-password"
            value={current}
            required
            onChange={(event) => setCurrent(event.target.value)}
          />
        </div>
        <div className="flex flex-col gap-1.5">
          <Label htmlFor="account-next">New password</Label>
          <Input
            id="account-next"
            type="password"
            autoComplete="new-password"
            value={next}
            required
            onChange={(event) => setNext(event.target.value)}
          />
        </div>
        <div className="flex flex-col gap-1.5">
          <Label htmlFor="account-confirm">Repeat new password</Label>
          <Input
            id="account-confirm"
            type="password"
            autoComplete="new-password"
            value={confirm}
            required
            onChange={(event) => setConfirm(event.target.value)}
          />
        </div>
        <Button type="submit" disabled={busy} className="self-start">
          <KeyRound />
          {busy ? "Saving…" : "Change password"}
        </Button>
      </form>
    </SectionCard>
  );
}

function TotpSection({
  state,
  required,
  onChanged,
  onError,
}: {
  state: TotpState | undefined;
  required: boolean;
  onChanged: () => Promise<void>;
  onError: (message: string) => void;
}) {
  const [enrolment, setEnrolment] = React.useState<TotpBegin | null>(null);
  const [code, setCode] = React.useState("");
  const [recovery, setRecovery] = React.useState<string[] | null>(null);
  const [password, setPassword] = React.useState("");
  const [busy, setBusy] = React.useState(false);

  if (recovery) {
    return (
      <div className="flex flex-col gap-3">
        <Notice title="Second factor enabled">
          Save these recovery codes now — each works once and they are not shown again.
        </Notice>
        <CodeBlock value={recovery.join("\n")} wrap={false} />
        <div className="flex gap-2">
          <CopyButton value={recovery.join("\n")} label="Copy codes" />
          <Button
            variant="ghost"
            size="sm"
            onClick={() => {
              setRecovery(null);
              setEnrolment(null);
            }}
          >
            Done
          </Button>
        </div>
      </div>
    );
  }

  if (enrolment) {
    return (
      <form
        className="flex flex-col gap-3"
        onSubmit={async (event) => {
          event.preventDefault();
          setBusy(true);
          try {
            const result = await panelApi<{ recovery_codes: string[] }>("/account/totp", {
              method: "PUT",
              body: { code },
            });
            setRecovery(result.recovery_codes);
            await onChanged();
          } catch (failure) {
            onError(failure instanceof Error ? failure.message : "Code was not accepted");
          } finally {
            setBusy(false);
          }
        }}
      >
        <CodeBlock value={enrolment.secret} />
        <div className="flex flex-wrap gap-2">
          <CopyButton value={enrolment.secret} label="Copy secret" />
          <CopyButton value={enrolment.uri} label="Copy otpauth URI" />
        </div>
        <div className="flex flex-col gap-1.5">
          <Label htmlFor="totp-code">Code from the application</Label>
          <Input
            id="totp-code"
            value={code}
            inputMode="numeric"
            maxLength={6}
            className="max-w-40 font-mono tracking-[0.35em]"
            onChange={(event) => setCode(event.target.value.replace(/\D/g, ""))}
          />
        </div>
        <Button type="submit" disabled={busy} className="self-start">
          <ShieldCheck />
          {busy ? "Verifying…" : "Confirm"}
        </Button>
      </form>
    );
  }

  if (state?.confirmed) {
    return (
      <div className="flex flex-col gap-3">
        <div className="flex items-center gap-2">
          <Badge variant="ok">enrolled</Badge>
          <span className="text-[13px] text-muted-foreground">
            {state.recovery_remaining} recovery code
            {state.recovery_remaining === 1 ? "" : "s"} remaining
          </span>
        </div>
        {required ? (
          <Notice tone="warn">
            <code>panel.require_totp</code> is set, so the second factor cannot be removed.
          </Notice>
        ) : (
          <form
            className="flex flex-col gap-2"
            onSubmit={async (event) => {
              event.preventDefault();
              setBusy(true);
              try {
                await panelApi("/account/totp", { method: "DELETE", body: { password } });
                setPassword("");
                await onChanged();
              } catch (failure) {
                onError(failure instanceof Error ? failure.message : "Could not disable");
              } finally {
                setBusy(false);
              }
            }}
          >
            <Label htmlFor="totp-password">Confirm with your password to remove it</Label>
            <div className="flex gap-2">
              <Input
                id="totp-password"
                type="password"
                value={password}
                required
                className="max-w-xs"
                onChange={(event) => setPassword(event.target.value)}
              />
              <Button type="submit" variant="danger" disabled={busy}>
                <ShieldOff />
                Remove
              </Button>
            </div>
          </form>
        )}
      </div>
    );
  }

  return (
    <div className="flex flex-col gap-3">
      <p className="text-[13px] text-muted-foreground">
        Not enrolled. Adding a second factor protects this account even if the password leaks.
      </p>
      <Button
        className="self-start"
        disabled={busy}
        onClick={async () => {
          setBusy(true);
          try {
            setEnrolment(await panelApi<TotpBegin>("/account/totp", { method: "POST" }));
          } catch (failure) {
            onError(failure instanceof Error ? failure.message : "Could not start enrolment");
          } finally {
            setBusy(false);
          }
        }}
      >
        <ShieldCheck />
        Enrol authenticator
      </Button>
    </div>
  );
}
