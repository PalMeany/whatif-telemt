import * as React from "react";
import { KeyRound, ShieldCheck } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Notice } from "@/components/ui/feedback";
import { ApiError } from "@/lib/api";
import { useSession } from "@/hooks/use-session";

export default function LoginPage() {
  const { login } = useSession();
  const [username, setUsername] = React.useState("");
  const [password, setPassword] = React.useState("");
  const [totp, setTotp] = React.useState("");
  const [recovery, setRecovery] = React.useState("");
  const [needsSecondFactor, setNeedsSecondFactor] = React.useState(false);
  const [useRecovery, setUseRecovery] = React.useState(false);
  const [error, setError] = React.useState<string | null>(null);
  const [busy, setBusy] = React.useState(false);

  async function submit(event: React.FormEvent) {
    event.preventDefault();
    setBusy(true);
    setError(null);
    try {
      await login({
        username,
        password,
        totp: useRecovery ? undefined : totp,
        recoveryCode: useRecovery ? recovery : undefined,
      });
    } catch (failure) {
      if (failure instanceof ApiError && failure.code === "totp_required") {
        setNeedsSecondFactor(true);
        setError(null);
      } else {
        setError(failure instanceof Error ? failure.message : "Sign-in failed");
      }
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="flex min-h-screen items-center justify-center bg-[var(--background)] px-4 py-10">
      {/* A single centred column: nothing on this screen competes with the form. */}
      <div className="w-full max-w-[26rem]">
        <div className="mb-8 flex items-center gap-2.5">
          <div className="flex size-8 items-center justify-center rounded-md bg-[var(--foreground)]">
            <span className="text-base font-bold leading-none text-[var(--background)]">t</span>
          </div>
          <div>
            <div className="text-[15px] font-semibold leading-tight tracking-tight">
              telemt panel
            </div>
            <div className="text-[11px] uppercase tracking-[0.14em] text-muted-foreground">
              control plane
            </div>
          </div>
        </div>

        <form onSubmit={submit} className="flex flex-col gap-4">
          <div className="flex flex-col gap-1.5">
            <Label htmlFor="username">Operator</Label>
            <Input
              id="username"
              value={username}
              autoComplete="username"
              autoFocus
              required
              onChange={(event) => setUsername(event.target.value)}
            />
          </div>
          <div className="flex flex-col gap-1.5">
            <Label htmlFor="password">Password</Label>
            <Input
              id="password"
              type="password"
              value={password}
              autoComplete="current-password"
              required
              onChange={(event) => setPassword(event.target.value)}
            />
          </div>

          {needsSecondFactor ? (
            useRecovery ? (
              <div className="flex flex-col gap-1.5">
                <Label htmlFor="recovery">Recovery code</Label>
                <Input
                  id="recovery"
                  value={recovery}
                  autoComplete="one-time-code"
                  placeholder="xxxxx-xxxxx"
                  className="font-mono"
                  onChange={(event) => setRecovery(event.target.value)}
                />
              </div>
            ) : (
              <div className="flex flex-col gap-1.5">
                <Label htmlFor="totp">One-time code</Label>
                <Input
                  id="totp"
                  value={totp}
                  inputMode="numeric"
                  autoComplete="one-time-code"
                  maxLength={6}
                  placeholder="000000"
                  className="font-mono tracking-[0.35em]"
                  autoFocus
                  onChange={(event) => setTotp(event.target.value.replace(/\D/g, ""))}
                />
              </div>
            )
          ) : null}

          {error ? (
            <Notice tone="danger" title="Sign-in failed">
              {error}
            </Notice>
          ) : null}

          <Button type="submit" disabled={busy} className="mt-1 w-full">
            {needsSecondFactor ? <ShieldCheck /> : <KeyRound />}
            {busy ? "Checking…" : needsSecondFactor ? "Verify and sign in" : "Sign in"}
          </Button>

          {needsSecondFactor ? (
            <button
              type="button"
              className="self-center text-[12px] text-muted-foreground underline-offset-4 hover:underline"
              onClick={() => setUseRecovery((value) => !value)}
            >
              {useRecovery ? "Use an authenticator code" : "Use a recovery code instead"}
            </button>
          ) : null}
        </form>

        <p className="mt-8 text-center text-[11px] leading-relaxed text-muted-foreground">
          Sessions are bound to this browser and expire on inactivity.
          <br />
          Every change made here is recorded in the audit log.
        </p>
      </div>
    </div>
  );
}
