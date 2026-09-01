/**
 * Panel API client.
 *
 * Two shapes travel over this transport. The panel's own routes answer
 * `{ ok, data }`; the relayed telemt Control API answers its own envelope
 * `{ ok, data, revision }` untouched, so the config editor still sees the
 * revision it needs for optimistic concurrency.
 */

const API_ROOT = "/panel/api";
const CLIENT_HEADER = "X-Telemt-Panel";
const CSRF_HEADER = "X-Telemt-Csrf";

export class ApiError extends Error {
  readonly code: string;
  readonly status: number;

  constructor(status: number, code: string, message: string) {
    super(message);
    this.name = "ApiError";
    this.code = code;
    this.status = status;
  }
}

/** CSRF token of the live session, replaced on every login. */
let csrfToken = "";

export function setCsrfToken(token: string) {
  csrfToken = token;
}

export function getCsrfToken() {
  return csrfToken;
}

type RequestOptions = {
  method?: string;
  body?: unknown;
  signal?: AbortSignal;
  /** Sends the body verbatim instead of encoding it as JSON. */
  rawBody?: string;
  contentType?: string;
  headers?: Record<string, string>;
};

async function request<T>(path: string, options: RequestOptions = {}): Promise<T> {
  const method = options.method ?? "GET";
  const headers: Record<string, string> = {
    [CLIENT_HEADER]: "1",
    Accept: "application/json",
    ...options.headers,
  };
  if (method !== "GET" && method !== "HEAD") {
    headers[CSRF_HEADER] = csrfToken;
  }
  let body: string | undefined;
  if (options.rawBody !== undefined) {
    body = options.rawBody;
    headers["Content-Type"] = options.contentType ?? "text/plain; charset=utf-8";
  } else if (options.body !== undefined) {
    body = JSON.stringify(options.body);
    headers["Content-Type"] = "application/json; charset=utf-8";
  }

  const response = await fetch(path, {
    method,
    headers,
    body,
    signal: options.signal,
    credentials: "same-origin",
    redirect: "error",
    cache: "no-store",
  });

  const text = await response.text();
  let payload: unknown = null;
  if (text.length > 0) {
    try {
      payload = JSON.parse(text);
    } catch {
      payload = null;
    }
  }

  if (!response.ok) {
    const envelope = payload as { error?: { code?: string; message?: string } } | null;
    throw new ApiError(
      response.status,
      envelope?.error?.code ?? "http_error",
      envelope?.error?.message ?? `Request failed with status ${response.status}`,
    );
  }
  const envelope = payload as { ok?: boolean; data?: T } | null;
  if (envelope && typeof envelope === "object" && "data" in envelope) {
    return envelope.data as T;
  }
  return payload as T;
}

/** Calls one of the panel's own routes. */
export function panelApi<T>(path: string, options: RequestOptions = {}): Promise<T> {
  return request<T>(`${API_ROOT}${path}`, options);
}

/** Full Control API answer, including the fields outside `data`. */
export type ControlEnvelope<T> = {
  ok: boolean;
  data: T;
  revision?: string;
};

/**
 * Calls the telemt Control API of one node through the relay.
 *
 * The Control API envelope is returned whole: `revision` is part of the
 * contract for every mutating config call.
 */
export async function controlApi<T>(
  nodeId: string,
  path: string,
  options: RequestOptions = {},
): Promise<ControlEnvelope<T>> {
  const separator = path.includes("?") ? "&" : "?";
  const target = `${API_ROOT}/control${path}${separator}node=${encodeURIComponent(nodeId)}`;
  const method = options.method ?? "GET";
  const headers: Record<string, string> = {
    [CLIENT_HEADER]: "1",
    Accept: "application/json",
    ...options.headers,
  };
  if (method !== "GET" && method !== "HEAD") {
    headers[CSRF_HEADER] = csrfToken;
  }
  let body: string | undefined;
  if (options.body !== undefined) {
    body = JSON.stringify(options.body);
    headers["Content-Type"] = "application/json; charset=utf-8";
  }
  const response = await fetch(target, {
    method,
    headers,
    body,
    signal: options.signal,
    credentials: "same-origin",
    redirect: "error",
    cache: "no-store",
  });
  const text = await response.text();
  let payload: unknown = null;
  if (text.length > 0) {
    try {
      payload = JSON.parse(text);
    } catch {
      payload = null;
    }
  }
  if (!response.ok) {
    const envelope = payload as { error?: { code?: string; message?: string } } | null;
    throw new ApiError(
      response.status,
      envelope?.error?.code ?? "http_error",
      envelope?.error?.message ?? `Request failed with status ${response.status}`,
    );
  }
  return payload as ControlEnvelope<T>;
}

/** Convenience wrapper that discards everything but the Control API `data`. */
export async function controlData<T>(
  nodeId: string,
  path: string,
  options: RequestOptions = {},
): Promise<T> {
  const envelope = await controlApi<T>(nodeId, path, options);
  return envelope.data;
}
