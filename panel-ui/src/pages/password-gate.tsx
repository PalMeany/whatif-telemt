import * as React from "react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { CodeBlock, CopyButton, Notice } from "@/components/ui/feedback";
import { panelApi } from "@/lib/api";
import { useSession } from "@/hooks/use-session";

type TotpBegin = { secret: string; uri: string; digits: number; period: number };

/**
 * The screen shown before the shell when the account is not yet usable.
 *
 * Both gates are also enforced server-side; this is the operator-facing half.
 */
export default function PasswordGate({ reason }: { reason: "password" | "totp" }) {
  const { refresh, logout } = useSession();
  return (
    <div className="flex min-h-screen items-center justify-center px-4 py-10">
      <div className="w-full max-w-[28rem]">
        {reason === "password" ? (
          <ChangePassword onDone={refresh} />
        ) : (
          <EnrolSecondFactor onDone={refresh} />
        )}
        <div className="mt-6 text-center">
          <button
            type="button"
            className="text-[12px] text-muted-foreground underline-offset-4 hover:underline"
            onClick={() => void logout()}
          >
            Sign out
          </button>
        </div>
      </div>
    </div>
  );
}

function ChangePassword({ onDone }: { onDone: () => Promise<void> }) {
  const [current, setCurrent] = React.useState("");
  const [next, setNext] = React.useState("");
  const [confirm, setConfirm] = React.useState("");
  const [error, setError] = React.useState<string | null>(null);
  const [busy, setBusy] = React.useState(false);

  async function submit(event: React.FormEvent) {
    event.preventDefault();
    if (next !== confirm) {
      setError("The two new passwords do not match");
      return;
    }
    setBusy(true);
    setError(null);
    try {
      await panelApi("/account/password", {
        method: "POST",
        body: { current_password: current, new_password: next },
      });
      await onDone();
    } catch (failure) {
      setError(failure instanceof Error ? failure.message : "Could not change the password");
    } finally {
      setBusy(false);
    }
  }

  return (
    <form onSubmit={submit} className="flex flex-col gap-4">
      <div>
        <h1 className="text-lg font-semibold tracking-tight">Set a new password</h1>
        <p className="mt-1 text-[13px] text-muted-foreground">
          This account still uses its provisional credential. Nothing else is reachable until it is
          replaced.
        </p>
      </div>
      <div className="flex flex-col gap-1.5">
        <Label htmlFor="current">Current password</Label>
        <Input
          id="current"
          type="password"
          autoComplete="current-password"
          value={current}
          required
          onChange={(event) => setCurrent(event.target.value)}
        />
      </div>
      <div className="flex flex-col gap-1.5">
        <Label htmlFor="next">New password</Label>
        <Input
          id="next"
          type="password"
          autoComplete="new-password"
          value={next}
          required
          onChange={(event) => setNext(event.target.value)}
        />
      </div>
      <div className="flex flex-col gap-1.5">
        <Label htmlFor="confirm">Repeat new password</Label>
        <Input
          id="confirm"
          type="password"
          autoComplete="new-password"
          value={confirm}
          required
          onChange={(event) => setConfirm(event.target.value)}
        />
      </div>
      {error ? <Notice tone="danger">{error}</Notice> : null}
      <Button type="submit" disabled={busy}>
        {busy ? "Saving…" : "Change password and continue"}
      </Button>
    </form>
  );
}

function EnrolSecondFactor({ onDone }: { onDone: () => Promise<void> }) {
  const [enrolment, setEnrolment] = React.useState<TotpBegin | null>(null);
  const [code, setCode] = React.useState("");
  const [recovery, setRecovery] = React.useState<string[] | null>(null);
  const [error, setError] = React.useState<string | null>(null);
  const [busy, setBusy] = React.useState(false);

  React.useEffect(() => {
    void (async () => {
      try {
        setEnrolment(await panelApi<TotpBegin>("/account/totp", { method: "POST" }));
      } catch (failure) {
        setError(failure instanceof Error ? failure.message : "Could not start enrolment");
      }
    })();
  }, []);

  async function confirm(event: React.FormEvent) {
    event.preventDefault();
    setBusy(true);
    setError(null);
    try {
      const result = await panelApi<{ recovery_codes: string[] }>("/account/totp", {
        method: "PUT",
        body: { code },
      });
      setRecovery(result.recovery_codes);
    } catch (failure) {
      setError(failure instanceof Error ? failure.message : "Code was not accepted");
    } finally {
      setBusy(false);
    }
  }

  if (recovery) {
    return (
      <div className="flex flex-col gap-4">
        <div>
          <h1 className="text-lg font-semibold tracking-tight">Save your recovery codes</h1>
          <p className="mt-1 text-[13px] text-muted-foreground">
            Each code works once. They are shown now and never again — only their hashes are
            stored.
          </p>
        </div>
        <CodeBlock value={recovery.join("\n")} wrap={false} />
        <div className="flex gap-2">
          <CopyButton value={recovery.join("\n")} label="Copy codes" />
        </div>
        <Button onClick={() => void onDone()}>I have saved them — continue</Button>
      </div>
    );
  }

  return (
    <form onSubmit={confirm} className="flex flex-col gap-4">
      <div>
        <h1 className="text-lg font-semibold tracking-tight">Enrol a second factor</h1>
        <p className="mt-1 text-[13px] text-muted-foreground">
          This panel requires two-factor authentication. Add the secret below to an authenticator
          application and confirm the code it shows.
        </p>
      </div>
      {enrolment ? (
        <>
          <div className="flex flex-col gap-2">
            <Label>Secret</Label>
            <CodeBlock value={enrolment.secret} />
            <div className="flex flex-wrap gap-2">
              <CopyButton value={enrolment.secret} label="Copy secret" />
              <CopyButton value={enrolment.uri} label="Copy otpauth URI" />
            </div>
          </div>
          <div className="flex flex-col gap-1.5">
            <Label htmlFor="code">Code from the application</Label>
            <Input
              id="code"
              value={code}
              inputMode="numeric"
              maxLength={6}
              placeholder="000000"
              className="font-mono tracking-[0.35em]"
              onChange={(event) => setCode(event.target.value.replace(/\D/g, ""))}
            />
          </div>
        </>
      ) : (
        <Notice>Preparing enrolment…</Notice>
      )}
      {error ? <Notice tone="danger">{error}</Notice> : null}
      <Button type="submit" disabled={busy || !enrolment}>
        {busy ? "Verifying…" : "Confirm and continue"}
      </Button>
    </form>
  );
}
