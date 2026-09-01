import { Check, ChevronsUpDown, Server, ShieldAlert } from "lucide-react";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { StatusDot } from "@/components/ui/feedback";
import { useNodes } from "@/hooks/use-node";
import { cn, formatRelative } from "@/lib/utils";

/** Fleet selector. Everything on the page follows whatever this names. */
export function NodeSwitcher({ className }: { className?: string }) {
  const { nodes, nodeId, node, setNodeId } = useNodes();
  const unreachable = nodes.filter((entry) => !entry.reachable).length;

  return (
    <DropdownMenu>
      <DropdownMenuTrigger
        className={cn(
          "group flex h-9 min-w-0 items-center gap-2 rounded-md border border-[var(--input)] px-2.5 text-left text-[13px] transition-colors hover:bg-[var(--accent)]",
          className,
        )}
      >
        <Server className="size-3.5 shrink-0 opacity-70" />
        <span className="min-w-0 flex-1 truncate font-medium">{node?.name ?? nodeId}</span>
        {unreachable > 0 ? (
          <ShieldAlert className="size-3.5 shrink-0 text-[var(--danger)]" />
        ) : null}
        <ChevronsUpDown className="size-3.5 shrink-0 opacity-50" />
      </DropdownMenuTrigger>
      <DropdownMenuContent align="start" className="min-w-[16rem]">
        <DropdownMenuLabel>Nodes</DropdownMenuLabel>
        {nodes.map((entry) => (
          <DropdownMenuItem
            key={entry.id}
            onSelect={() => setNodeId(entry.id)}
            className="flex items-center gap-2"
          >
            <StatusDot state={entry.reachable ? "ok" : "danger"} />
            <span className="min-w-0 flex-1 truncate">{entry.name}</span>
            <span className="shrink-0 text-[11px] text-muted-foreground">
              {entry.kind === "local" ? "this host" : formatRelative(entry.checked_at)}
            </span>
            {entry.id === nodeId ? <Check className="size-3.5 shrink-0" /> : null}
          </DropdownMenuItem>
        ))}
        {nodes.length === 0 ? (
          <div className="px-2 py-3 text-[12px] text-muted-foreground">No nodes yet</div>
        ) : null}
        <DropdownMenuSeparator />
        <DropdownMenuItem asChild>
          <a href="/fleet" className="block w-full">
            Manage fleet
          </a>
        </DropdownMenuItem>
      </DropdownMenuContent>
    </DropdownMenu>
  );
}
