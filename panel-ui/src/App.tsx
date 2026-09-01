import { Navigate, Route, Routes } from "react-router-dom";
import { AppShell } from "@/components/layout/app-shell";
import { NodeProvider } from "@/hooks/use-node";
import { useSession } from "@/hooks/use-session";
import { Spinner } from "@/components/ui/feedback";
import LoginPage from "@/pages/login";
import PasswordGate from "@/pages/password-gate";
import OverviewPage from "@/pages/overview";
import UsersPage from "@/pages/users";
import TrafficPage from "@/pages/traffic";
import MiddleEndPage from "@/pages/middle-end";
import UpstreamsPage from "@/pages/upstreams";
import SecurityPage from "@/pages/security";
import RuntimePage from "@/pages/runtime";
import EventsPage from "@/pages/events";
import ConfigPage from "@/pages/config";
import FleetPage from "@/pages/fleet";
import OperatorsPage from "@/pages/operators";
import AuditPage from "@/pages/audit";
import SettingsPage from "@/pages/settings";
import AccountPage from "@/pages/account";

export default function App() {
  const { status, session } = useSession();

  if (status === "loading") {
    return (
      <div className="flex h-screen items-center justify-center gap-2 text-muted-foreground">
        <Spinner />
        <span className="text-sm">Loading panel…</span>
      </div>
    );
  }

  if (status === "anonymous" || !session) {
    return <LoginPage />;
  }

  // Two gates run before the application shell: a provisional password has to
  // be replaced, and an enrolment requirement has to be satisfied. Both are
  // enforced by the server as well; this only avoids rendering a shell whose
  // every request would be refused.
  if (session.must_change_password) {
    return <PasswordGate reason="password" />;
  }
  if (session.totp_required && !session.totp_enabled) {
    return <PasswordGate reason="totp" />;
  }

  return (
    <NodeProvider>
      <Routes>
        <Route element={<AppShell />}>
          <Route index element={<OverviewPage />} />
          <Route path="/users" element={<UsersPage />} />
          <Route path="/traffic" element={<TrafficPage />} />
          <Route path="/middle-end" element={<MiddleEndPage />} />
          <Route path="/upstreams" element={<UpstreamsPage />} />
          <Route path="/security" element={<SecurityPage />} />
          <Route path="/runtime" element={<RuntimePage />} />
          <Route path="/events" element={<EventsPage />} />
          <Route path="/config" element={<ConfigPage />} />
          <Route path="/fleet" element={<FleetPage />} />
          <Route path="/operators" element={<OperatorsPage />} />
          <Route path="/audit" element={<AuditPage />} />
          <Route path="/settings" element={<SettingsPage />} />
          <Route path="/account" element={<AccountPage />} />
          <Route path="*" element={<Navigate to="/" replace />} />
        </Route>
      </Routes>
    </NodeProvider>
  );
}
