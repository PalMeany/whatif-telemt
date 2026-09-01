import * as React from "react";
import { ApiError, panelApi, setCsrfToken } from "@/lib/api";
import type { BootstrapView, SessionView } from "@/lib/types";

type LoginInput = {
  username: string;
  password: string;
  totp?: string;
  recoveryCode?: string;
};

type SessionState = {
  status: "loading" | "anonymous" | "authenticated";
  session: SessionView | null;
  bootstrap: BootstrapView | null;
  login: (input: LoginInput) => Promise<void>;
  logout: () => Promise<void>;
  refresh: () => Promise<void>;
};

const SessionContext = React.createContext<SessionState | null>(null);

export function SessionProvider({ children }: { children: React.ReactNode }) {
  const [status, setStatus] = React.useState<SessionState["status"]>("loading");
  const [session, setSession] = React.useState<SessionView | null>(null);
  const [bootstrap, setBootstrap] = React.useState<BootstrapView | null>(null);

  const load = React.useCallback(async () => {
    try {
      const current = await panelApi<SessionView>("/session");
      setCsrfToken(current.csrf_token);
      setSession(current);
      setStatus("authenticated");
      try {
        setBootstrap(await panelApi<BootstrapView>("/bootstrap"));
      } catch {
        // A forced password change blocks /bootstrap by design; the session
        // alone is enough to render the change-password gate.
        setBootstrap(null);
      }
    } catch (error) {
      if (error instanceof ApiError && error.status === 401) {
        setStatus("anonymous");
        setSession(null);
        setBootstrap(null);
        setCsrfToken("");
        return;
      }
      setStatus("anonymous");
      setSession(null);
    }
  }, []);

  React.useEffect(() => {
    void load();
  }, [load]);

  const login = React.useCallback(
    async (input: LoginInput) => {
      const created = await panelApi<SessionView>("/session", {
        method: "POST",
        body: {
          username: input.username,
          password: input.password,
          totp: input.totp || undefined,
          recovery_code: input.recoveryCode || undefined,
        },
      });
      setCsrfToken(created.csrf_token);
      setSession(created);
      setStatus("authenticated");
      await load();
    },
    [load],
  );

  const logout = React.useCallback(async () => {
    try {
      await panelApi("/session", { method: "DELETE" });
    } finally {
      setCsrfToken("");
      setSession(null);
      setBootstrap(null);
      setStatus("anonymous");
    }
  }, []);

  const value = React.useMemo<SessionState>(
    () => ({ status, session, bootstrap, login, logout, refresh: load }),
    [status, session, bootstrap, login, logout, load],
  );

  return <SessionContext.Provider value={value}>{children}</SessionContext.Provider>;
}

export function useSession(): SessionState {
  const context = React.useContext(SessionContext);
  if (!context) throw new Error("useSession must be used inside SessionProvider");
  return context;
}

/** True when the caller's role carries at least the required level. */
export function useCan(level: "operator" | "admin"): boolean {
  const { session } = useSession();
  if (!session) return false;
  if (session.role === "admin") return true;
  return level === "operator" && session.role === "operator";
}
