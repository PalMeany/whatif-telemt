import type { ReactNode } from "react";
import { Notice } from "@/components/ui/feedback";
import { edgeEnabled, edgeReason, type EdgeEnvelope } from "@/lib/edge";

/**
 * Renders the node's own explanation instead of an empty view.
 *
 * A disabled runtime-edge feature and an empty result look identical once the
 * payload is gone, and telling an operator "no data" when the answer is "you
 * did not switch it on" is the difference between a five-minute fix and a
 * bug report.
 */
export function EdgeGate({
  envelope,
  feature,
  hint,
  children,
}: {
  envelope: EdgeEnvelope<unknown> | undefined;
  feature: string;
  hint?: string;
  children: ReactNode;
}) {
  if (envelope && !edgeEnabled(envelope)) {
    return (
      <Notice tone="warn" title={`${feature} is not available on this node`}>
        {edgeReason(envelope) ?? hint ?? "The node reported the feature as disabled."}
      </Notice>
    );
  }
  return <>{children}</>;
}
