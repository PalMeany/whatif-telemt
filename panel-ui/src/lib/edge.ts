/**
 * Runtime-edge envelope helpers.
 *
 * The Control API's runtime-edge endpoints answer
 * `{ enabled, reason?, generated_at_epoch_secs, data: { ... } }` — the payload
 * sits one level below the availability flag, because the flag is what a node
 * can report when the feature is switched off and there is no payload at all.
 */

export type EdgeEnvelope<T> = {
  enabled?: boolean;
  middle_proxy_enabled?: boolean;
  reason?: string;
  generated_at_epoch_secs?: number;
  data?: T;
};

/** Returns the inner payload, or undefined when the node reported none. */
export function edgeData<T>(envelope: EdgeEnvelope<T> | undefined): T | undefined {
  return envelope?.data;
}

/** True when the node has the feature switched on. */
export function edgeEnabled(envelope: EdgeEnvelope<unknown> | undefined): boolean {
  if (!envelope) return true;
  if (envelope.enabled !== undefined) return envelope.enabled;
  if (envelope.middle_proxy_enabled !== undefined) return envelope.middle_proxy_enabled;
  return true;
}

/** Why the node reported the feature as unavailable, when it said. */
export function edgeReason(envelope: EdgeEnvelope<unknown> | undefined): string | undefined {
  const reason = envelope?.reason;
  return reason && reason.length > 0 ? reason : undefined;
}
