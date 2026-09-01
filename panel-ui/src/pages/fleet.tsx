import * as React from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Ellipsis, Link2, Plus, RotateCw, Trash2 } from "lucide-react";
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
import { Input, Textarea } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { CodeBlock, CopyButton, Notice, StatusDot } from "@/components/ui/feedback";
import { PageSection, QueryState, RefreshButton, SectionCard } from "@/components/page";
import { panelApi } from "@/lib/api";
import { useSession } from "@/hooks/use-session";
import { useNodes } from "@/hooks/use-node";
import { formatRelative, formatTime } from "@/lib/utils";
import type { NodeView } from "@/lib/types";

type LinkTokenView = {
  token: string;
  node_id: string;
  node_name: string;
  url: string;
  fingerprint: string | null;
};

export default function FleetPage() {
  const { bootstrap, session } = useSession();
  const { nodes, refetch } = useNodes();
  const queryClient = useQueryClient();
  const isAdmin = session?.role === "admin";
  const [linking, setLinking] = React.useState(false);
  const [error, setError] = React.useState<string | null>(null);

  const invalidate = React.useCallback(() => {
    void queryClient.invalidateQueries({ queryKey: ["nodes"] });
    void queryClient.invalidateQueries({ queryKey: ["overview"] });
    refetch();
  }, [queryClient, refetch]);

  const probe = useMutation({
    mutationFn: (id: string) =>
      panelApi<{ reachable: boolean; error?: string }>(`/nodes/${encodeURIComponent(id)}/probe`, {
        method: "POST",
      }),
    onSuccess: (result) => {
      setError(result.reachable ? null : (result.error ?? "Node did not answer"));
      invalidate();
    },
    onError: (failure: Error) => setError(failure.message),
  });

  const unlink = useMutation({
    mutationFn: (id: string) =>
      panelApi(`/nodes/${encodeURIComponent(id)}`, { method: "DELETE" }),
    onSuccess: () => {
      setError(null);
      invalidate();
    },
    onError: (failure: Error) => setError(failure.message),
  });

  const isMaster = bootstrap?.node.is_master ?? false;
  const isAgent = bootstrap?.node.is_agent ?? false;

  return (
    <div className="flex flex-col gap-5">
      <PageSection
        description="Nodes this panel drives, and how this node itself can be linked into another panel."
        actions={
          <>
            <RefreshButton onClick={invalidate} />
            {isAdmin && isMaster ? (
              <Button size="sm" onClick={() => setLinking(true)}>
                <Plus />
                Link node
              </Button>
            ) : null}
          </>
        }
      >
        {error ? (
          <Notice tone="danger" title="Fleet action failed">
            {error}
          </Notice>
        ) : null}
        {!bootstrap?.node.cluster_enabled ? (
          <Notice tone="warn" title="Federation is off on this node">
            Set <code>panel.cluster.enabled = true</code> and a role of <code>master</code>,{" "}
            <code>agent</code>, or <code>master-agent</code> to link nodes together.
          </Notice>
        ) : null}

        <SectionCard bodyClassName="px-0 pb-0">
          <Table>
            <TableHeader>
              <TableRow>
                <TableHead>Node</TableHead>
                <TableHead>Endpoint</TableHead>
                <TableHead>State</TableHead>
                <TableHead className="text-right">Latency</TableHead>
                <TableHead>Version</TableHead>
                <TableHead className="w-10" />
              </TableRow>
            </TableHeader>
            <TableBody>
              {nodes.map((node) => (
                <TableRow key={node.id}>
                  <TableCell>
                    <div className="flex items-center gap-2">
                      <span className="font-medium">{node.name}</span>
                      {node.kind === "local" ? <Badge variant="outline">this host</Badge> : null}
                      {node.pinned ? <Badge variant="ok">pinned</Badge> : null}
                    </div>
                    <div className="font-mono text-[11px] text-muted-foreground">{node.id}</div>
                    {node.tags.length > 0 ? (
                      <div className="mt-1 flex flex-wrap gap-1">
                        {node.tags.map((tag) => (
                          <Badge key={tag} variant="outline">
                            {tag}
                          </Badge>
                        ))}
                      </div>
                    ) : null}
                  </TableCell>
                  <TableCell className="font-mono text-[12px] text-muted-foreground">
                    {node.url ?? "in-process"}
                  </TableCell>
                  <TableCell>
                    <StatusDot
                      state={node.reachable ? "ok" : "danger"}
                      label={node.reachable ? "reachable" : (node.error ?? "unreachable")}
                    />
                    {node.kind === "linked" ? (
                      <div className="text-[11px] text-muted-foreground">
                        checked {formatRelative(node.checked_at)}
                      </div>
                    ) : null}
                  </TableCell>
                  <TableCell className="tabular text-right">
                    {node.latency_ms !== null ? `${node.latency_ms} ms` : "—"}
                  </TableCell>
                  <TableCell className="font-mono text-[12px]">{node.version ?? "—"}</TableCell>
                  <TableCell className="text-right">
                    {node.kind === "linked" ? (
                      <DropdownMenu>
                        <DropdownMenuTrigger asChild>
                          <Button variant="ghost" size="icon-sm" aria-label={`Actions for ${node.name}`}>
                            <Ellipsis />
                          </Button>
                        </DropdownMenuTrigger>
                        <DropdownMenuContent align="end">
                          <DropdownMenuItem onSelect={() => probe.mutate(node.id)}>
                            <RotateCw />
                            Probe now
                          </DropdownMenuItem>
                          {isAdmin ? (
                            <>
                              <DropdownMenuSeparator />
                              <DropdownMenuItem
                                destructive
                                onSelect={() => {
                                  if (window.confirm(`Unlink "${node.name}"?`)) {
                                    unlink.mutate(node.id);
                                  }
                                }}
                              >
                                <Trash2 />
                                Unlink
                              </DropdownMenuItem>
                            </>
                          ) : null}
                        </DropdownMenuContent>
                      </DropdownMenu>
                    ) : null}
                  </TableCell>
                </TableRow>
              ))}
            </TableBody>
          </Table>
        </SectionCard>
      </PageSection>

      {isAgent && isAdmin ? <LinkTokenCard /> : null}

      <LinkNodeDialog
        open={linking}
        onOpenChange={setLinking}
        onLinked={() => {
          setLinking(false);
          invalidate();
        }}
      />
    </div>
  );
}

function LinkTokenCard() {
  const [revealed, setRevealed] = React.useState(false);
  const queryClient = useQueryClient();
  const token = useQuery({
    queryKey: ["link-token"],
    queryFn: () => panelApi<LinkTokenView>("/nodes/link-token"),
    retry: false,
    enabled: revealed,
  });
  const rotate = useMutation({
    mutationFn: () => panelApi("/nodes/link-token/rotate", { method: "POST" }),
    onSuccess: () => void queryClient.invalidateQueries({ queryKey: ["link-token"] }),
  });

  return (
    <SectionCard
      title="This node's link token"
      description="Paste it into a master panel to bring this node into its fleet."
      actions={
        <>
          {revealed ? (
            <Button
              variant="outline"
              size="sm"
              onClick={() => {
                if (
                  window.confirm(
                    "Rotating the link key invalidates every existing link to this node. Continue?",
                  )
                ) {
                  rotate.mutate();
                }
              }}
            >
              <RotateCw />
              Rotate key
            </Button>
          ) : null}
          <Button variant={revealed ? "ghost" : "outline"} size="sm" onClick={() => setRevealed((value) => !value)}>
            <Link2 />
            {revealed ? "Hide" : "Reveal token"}
          </Button>
        </>
      }
    >
      {!revealed ? (
        <p className="text-[13px] text-muted-foreground">
          The token carries this node's HMAC link key. It is hidden until you ask for it.
        </p>
      ) : (
        <QueryState isLoading={token.isLoading} error={token.error}>
          <div className="flex flex-col gap-3">
            <CodeBlock value={token.data?.token ?? ""} />
            <div className="flex flex-wrap gap-2">
              <CopyButton value={token.data?.token ?? ""} label="Copy token" />
            </div>
            <div className="grid gap-1 text-[12px] text-muted-foreground sm:grid-cols-2">
              <div>
                Node id: <span className="font-mono">{token.data?.node_id}</span>
              </div>
              <div>
                Endpoint: <span className="font-mono">{token.data?.url}</span>
              </div>
              <div className="sm:col-span-2">
                Certificate pin:{" "}
                <span className="font-mono break-all">
                  {token.data?.fingerprint ?? "none — the master will use web PKI"}
                </span>
              </div>
            </div>
          </div>
        </QueryState>
      )}
    </SectionCard>
  );
}

function LinkNodeDialog({
  open,
  onOpenChange,
  onLinked,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  onLinked: () => void;
}) {
  const [token, setToken] = React.useState("");
  const [name, setName] = React.useState("");
  const [tags, setTags] = React.useState("");
  const [error, setError] = React.useState<string | null>(null);
  const [busy, setBusy] = React.useState(false);
  const [linked, setLinked] = React.useState<NodeView | null>(null);

  React.useEffect(() => {
    if (open) {
      setToken("");
      setName("");
      setTags("");
      setError(null);
      setLinked(null);
    }
  }, [open]);

  async function submit(event: React.FormEvent) {
    event.preventDefault();
    setBusy(true);
    setError(null);
    try {
      const result = await panelApi<NodeView>("/nodes", {
        method: "POST",
        body: {
          token: token.trim(),
          name: name.trim() || undefined,
          tags: tags
            .split(",")
            .map((tag) => tag.trim())
            .filter(Boolean),
        },
      });
      setLinked(result);
      onLinked();
    } catch (failure) {
      setError(failure instanceof Error ? failure.message : "The node could not be linked");
    } finally {
      setBusy(false);
    }
  }

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent>
        <form onSubmit={submit}>
          <DialogHeader>
            <DialogTitle>Link a node</DialogTitle>
            <DialogDescription>
              Paste the token from the other node's Fleet page. The link is proven before it is
              stored, so a wrong token fails here rather than at first use.
            </DialogDescription>
          </DialogHeader>
          <DialogBody>
            <div className="flex flex-col gap-1.5">
              <Label htmlFor="link-token">Link token</Label>
              <Textarea
                id="link-token"
                rows={4}
                value={token}
                required
                spellCheck={false}
                placeholder="telemt-node:…"
                onChange={(event) => setToken(event.target.value)}
              />
            </div>
            <div className="grid gap-3 sm:grid-cols-2">
              <div className="flex flex-col gap-1.5">
                <Label htmlFor="link-name">Display name (optional)</Label>
                <Input
                  id="link-name"
                  value={name}
                  onChange={(event) => setName(event.target.value)}
                />
              </div>
              <div className="flex flex-col gap-1.5">
                <Label htmlFor="link-tags">Tags, comma separated</Label>
                <Input
                  id="link-tags"
                  value={tags}
                  placeholder="eu, edge"
                  onChange={(event) => setTags(event.target.value)}
                />
              </div>
            </div>
            {linked ? (
              <Notice title="Linked">
                {linked.name} answered at {formatTime(Math.floor(Date.now() / 1000))}.
              </Notice>
            ) : null}
            {error ? <Notice tone="danger">{error}</Notice> : null}
          </DialogBody>
          <DialogFooter>
            <Button type="button" variant="ghost" onClick={() => onOpenChange(false)}>
              Close
            </Button>
            <Button type="submit" disabled={busy}>
              {busy ? "Verifying…" : "Link node"}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}
