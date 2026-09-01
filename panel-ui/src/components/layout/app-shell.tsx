import * as React from "react";
import { NavLink, Outlet, useLocation } from "react-router-dom";
import {
  Activity,
  BookLock,
  Cable,
  ChevronRight,
  Cog,
  FileCode2,
  Fingerprint,
  Gauge,
  LayoutGrid,
  LogOut,
  Menu,
  Moon,
  Network,
  ScrollText,
  Server,
  Sun,
  UserCog,
  Users,
} from "lucide-react";
import { Button } from "@/components/ui/button";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { Badge } from "@/components/ui/badge";
import { NodeSwitcher } from "./node-switcher";
import { useSession } from "@/hooks/use-session";
import { useNodes } from "@/hooks/use-node";
import { cn } from "@/lib/utils";
import { useTheme } from "@/hooks/use-theme";

type NavItem = {
  to: string;
  label: string;
  icon: React.ComponentType<{ className?: string }>;
  level?: "operator" | "admin";
};

const NODE_NAV: NavItem[] = [
  { to: "/", label: "Overview", icon: LayoutGrid },
  { to: "/users", label: "Users", icon: Users },
  { to: "/traffic", label: "Traffic", icon: Activity },
  { to: "/middle-end", label: "Middle-End", icon: Cable },
  { to: "/upstreams", label: "Upstreams", icon: Network },
  { to: "/security", label: "Security", icon: Fingerprint },
  { to: "/runtime", label: "Runtime", icon: Gauge },
  { to: "/events", label: "Events", icon: ScrollText },
  { to: "/config", label: "Config", icon: FileCode2, level: "admin" },
];

const PANEL_NAV: NavItem[] = [
  { to: "/fleet", label: "Fleet", icon: Server },
  { to: "/operators", label: "Operators", icon: UserCog, level: "admin" },
  { to: "/audit", label: "Audit", icon: BookLock, level: "admin" },
  { to: "/settings", label: "Settings", icon: Cog },
];

export function AppShell() {
  const { session, bootstrap, logout } = useSession();
  const { nodes } = useNodes();
  const location = useLocation();
  const [mobileOpen, setMobileOpen] = React.useState(false);
  const { theme, setTheme } = useTheme();

  React.useEffect(() => setMobileOpen(false), [location.pathname]);

  const allowed = React.useCallback(
    (item: NavItem) => {
      if (!item.level) return true;
      if (!session) return false;
      if (session.role === "admin") return true;
      return item.level === "operator" && session.role === "operator";
    },
    [session],
  );

  const unreachable = nodes.filter((node) => !node.reachable).length;

  return (
    <div className="flex h-full min-h-screen bg-[var(--background)]">
      <aside
        className={cn(
          "fixed inset-y-0 left-0 z-40 flex w-60 flex-col border-r border-[var(--border)] bg-[var(--card)] transition-transform lg:static lg:translate-x-0",
          mobileOpen ? "translate-x-0" : "-translate-x-full",
        )}
      >
        <div className="flex h-14 items-center gap-2 border-b border-[var(--border)] px-4">
          <div className="flex size-6 items-center justify-center rounded bg-[var(--foreground)]">
            <span className="text-[13px] font-bold leading-none text-[var(--background)]">t</span>
          </div>
          <div className="min-w-0">
            <div className="truncate text-[13px] font-semibold leading-tight">telemt</div>
            <div className="truncate text-[10px] uppercase tracking-[0.12em] text-muted-foreground">
              {bootstrap?.node.role ?? "panel"}
            </div>
          </div>
        </div>

        <div className="border-b border-[var(--border)] p-3">
          <NodeSwitcher className="w-full" />
        </div>

        <nav className="flex-1 overflow-y-auto px-2 py-3">
          <NavGroup title="Node">
            {NODE_NAV.filter(allowed).map((item) => (
              <NavRow key={item.to} item={item} />
            ))}
          </NavGroup>
          <NavGroup title="Panel">
            {PANEL_NAV.filter(allowed).map((item) => (
              <NavRow
                key={item.to}
                item={item}
                badge={item.to === "/fleet" && unreachable > 0 ? unreachable : undefined}
              />
            ))}
          </NavGroup>
        </nav>

        <div className="border-t border-[var(--border)] p-2">
          <DropdownMenu>
            <DropdownMenuTrigger className="flex w-full items-center gap-2 rounded-md px-2 py-2 text-left transition-colors hover:bg-[var(--accent)]">
              <div className="flex size-7 shrink-0 items-center justify-center rounded-full border border-[var(--input)] text-[11px] font-semibold uppercase">
                {session?.username.slice(0, 2) ?? "··"}
              </div>
              <div className="min-w-0 flex-1">
                <div className="truncate text-[13px] font-medium leading-tight">
                  {session?.username}
                </div>
                <div className="truncate text-[11px] text-muted-foreground">{session?.role}</div>
              </div>
              <ChevronRight className="size-3.5 opacity-50" />
            </DropdownMenuTrigger>
            <DropdownMenuContent align="start" side="top" className="w-56">
              <DropdownMenuLabel>Signed in as {session?.username}</DropdownMenuLabel>
              <DropdownMenuItem asChild>
                <a href="/account">Account &amp; security</a>
              </DropdownMenuItem>
              <DropdownMenuItem
                onSelect={() => setTheme(theme === "dark" ? "light" : "dark")}
                className="flex items-center gap-2"
              >
                {theme === "dark" ? <Sun /> : <Moon />}
                {theme === "dark" ? "Light theme" : "Dark theme"}
              </DropdownMenuItem>
              <DropdownMenuSeparator />
              <DropdownMenuItem destructive onSelect={() => void logout()}>
                <LogOut />
                Sign out
              </DropdownMenuItem>
            </DropdownMenuContent>
          </DropdownMenu>
        </div>
      </aside>

      {mobileOpen ? (
        <button
          type="button"
          aria-label="Close navigation"
          className="fixed inset-0 z-30 bg-black/60 lg:hidden"
          onClick={() => setMobileOpen(false)}
        />
      ) : null}

      <div className="flex min-w-0 flex-1 flex-col">
        <header className="sticky top-0 z-20 flex h-14 items-center gap-3 border-b border-[var(--border)] bg-[var(--background)]/85 px-4 backdrop-blur lg:px-6">
          <Button
            variant="ghost"
            size="icon-sm"
            className="lg:hidden"
            onClick={() => setMobileOpen(true)}
            aria-label="Open navigation"
          >
            <Menu />
          </Button>
          <div className="min-w-0 flex-1">
            <h1 className="truncate text-[15px] font-semibold tracking-tight">
              {titleFor(location.pathname)}
            </h1>
          </div>
          {bootstrap ? (
            <Badge variant="outline" className="hidden font-mono sm:inline-flex">
              v{bootstrap.version}
            </Badge>
          ) : null}
        </header>
        <main className="min-w-0 flex-1 px-4 py-5 lg:px-6 lg:py-6">
          <Outlet />
        </main>
      </div>
    </div>
  );
}

function NavGroup({ title, children }: { title: string; children: React.ReactNode }) {
  return (
    <div className="mb-4 last:mb-0">
      <div className="px-2 pb-1.5 text-[10px] font-medium uppercase tracking-[0.12em] text-muted-foreground/70">
        {title}
      </div>
      <div className="flex flex-col gap-0.5">{children}</div>
    </div>
  );
}

function NavRow({ item, badge }: { item: NavItem; badge?: number }) {
  const Icon = item.icon;
  return (
    <NavLink
      to={item.to}
      end={item.to === "/"}
      className={({ isActive }) =>
        cn(
          "flex items-center gap-2.5 rounded-md px-2 py-1.5 text-[13px] transition-colors",
          isActive
            ? "bg-[var(--accent)] font-medium text-[var(--foreground)]"
            : "text-muted-foreground hover:bg-[var(--accent)]/60 hover:text-[var(--foreground)]",
        )
      }
    >
      <Icon className="size-4 shrink-0" />
      <span className="min-w-0 flex-1 truncate">{item.label}</span>
      {badge ? (
        <span className="tabular rounded-full bg-[var(--danger)] px-1.5 text-[10px] font-semibold text-black">
          {badge}
        </span>
      ) : null}
    </NavLink>
  );
}

const TITLES: Record<string, string> = {
  "/": "Overview",
  "/users": "Users",
  "/traffic": "Traffic",
  "/middle-end": "Middle-End pool",
  "/upstreams": "Upstreams",
  "/security": "Security",
  "/runtime": "Runtime",
  "/events": "Events",
  "/config": "Configuration",
  "/fleet": "Fleet",
  "/operators": "Operators",
  "/audit": "Audit log",
  "/settings": "Settings",
  "/account": "Account & security",
};

function titleFor(pathname: string): string {
  return TITLES[pathname] ?? "telemt panel";
}
