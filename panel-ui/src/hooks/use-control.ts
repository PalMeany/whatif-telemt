import { useQuery, type UseQueryOptions } from "@tanstack/react-query";
import { controlData } from "@/lib/api";
import { useNodeId } from "./use-node";

/**
 * Reads one Control API endpoint on the currently selected node.
 *
 * The node identifier is part of the query key, so switching nodes replaces the
 * data rather than showing another node's numbers under the new name.
 */
export function useControl<T>(
  path: string,
  options?: Partial<UseQueryOptions<T, Error, T, (string | undefined)[]>>,
) {
  const nodeId = useNodeId();
  return useQuery<T, Error, T, (string | undefined)[]>({
    queryKey: ["control", nodeId, path],
    queryFn: ({ signal }) => controlData<T>(nodeId, path, { signal }),
    staleTime: 3_000,
    retry: false,
    ...options,
  });
}
