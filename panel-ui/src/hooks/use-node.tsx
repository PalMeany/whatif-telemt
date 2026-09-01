import * as React from "react";
import { useQuery } from "@tanstack/react-query";
import { panelApi } from "@/lib/api";
import type { NodeView } from "@/lib/types";
import { useSession } from "./use-session";

const STORAGE_KEY = "telemt.panel.node";

type NodeState = {
  nodes: NodeView[];
  nodeId: string;
  node: NodeView | undefined;
  setNodeId: (id: string) => void;
  isLoading: boolean;
  refetch: () => void;
};

const NodeContext = React.createContext<NodeState | null>(null);

export function NodeProvider({ children }: { children: React.ReactNode }) {
  const { bootstrap, status } = useSession();
  const [nodeId, setNodeIdState] = React.useState<string>(
    () => window.localStorage.getItem(STORAGE_KEY) ?? "local",
  );

  const query = useQuery({
    queryKey: ["nodes"],
    queryFn: () => panelApi<{ nodes: NodeView[] }>("/nodes"),
    enabled: status === "authenticated",
    refetchInterval: 30_000,
  });

  const nodes = query.data?.nodes ?? [];

  React.useEffect(() => {
    if (!bootstrap?.default_node_id) return;
    if (window.localStorage.getItem(STORAGE_KEY)) return;
    setNodeIdState(bootstrap.default_node_id);
  }, [bootstrap?.default_node_id]);

  React.useEffect(() => {
    if (nodes.length === 0) return;
    if (nodes.some((node) => node.id === nodeId)) return;
    // A node that was unlinked while selected must not leave every page
    // requesting a target that no longer exists.
    setNodeIdState("local");
  }, [nodes, nodeId]);

  const setNodeId = React.useCallback((id: string) => {
    window.localStorage.setItem(STORAGE_KEY, id);
    setNodeIdState(id);
  }, []);

  const value = React.useMemo<NodeState>(
    () => ({
      nodes,
      nodeId,
      node: nodes.find((node) => node.id === nodeId),
      setNodeId,
      isLoading: query.isLoading,
      refetch: () => void query.refetch(),
    }),
    [nodes, nodeId, setNodeId, query],
  );

  return <NodeContext.Provider value={value}>{children}</NodeContext.Provider>;
}

export function useNodes(): NodeState {
  const context = React.useContext(NodeContext);
  if (!context) throw new Error("useNodes must be used inside NodeProvider");
  return context;
}

/** The identifier every Control API call on the current page should target. */
export function useNodeId(): string {
  return useNodes().nodeId;
}
