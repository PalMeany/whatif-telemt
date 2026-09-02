import { authorize, isPublic, isSealed, throttle, throttleIdentity } from "./auth.js";
import {
  blockingResponse,
  diagnosticsResponse,
  errorResponse,
  modelsResponse,
  normaliseRequest,
  RequestError,
  streamingResponse,
} from "./openai.js";
import { APP_CSS, APP_JS, FAVICON, indexHtml } from "./site.js";
import { upstreamConfig, UpstreamError } from "./upstream.js";

/**
 * One handler, `(Request, env) => Promise<Response>`, written against Web APIs
 * only. Cloudflare Workers, Vercel Edge and a Node runtime all provide that
 * surface, so the platform adapters in ../adapters are three files of glue and
 * nothing else lives in them.
 */

const MAX_BODY_BYTES = 1_000_000;

/**
 * The page loads its own script and stylesheet and talks to its own origin.
 * Nothing else is allowed, and there is no inline exception to carve out
 * because the site ships no inline script or style.
 */
const CSP =
  "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; " +
  "connect-src 'self'; font-src 'self'; form-action 'none'; frame-ancestors 'none'; " +
  "base-uri 'none'";

function securityHeaders(extra = {}) {
  return {
    "x-content-type-options": "nosniff",
    "x-frame-options": "DENY",
    "referrer-policy": "no-referrer",
    ...extra,
  };
}

/** Origins allowed to call the API from a browser. Empty means same-origin. */
function corsOrigin(request, env) {
  const allowed = String(env.ASSISTANT_CORS_ORIGINS || "")
    .split(",")
    .map((value) => value.trim())
    .filter(Boolean);
  if (allowed.length === 0) return null;
  const origin = request.headers.get("origin");
  if (!origin) return null;
  if (allowed.includes("*")) return "*";
  return allowed.includes(origin) ? origin : null;
}

function corsHeaders(origin) {
  if (!origin) return {};
  return {
    "access-control-allow-origin": origin,
    "access-control-allow-headers": "authorization, content-type, x-api-key",
    "access-control-allow-methods": "GET, POST, OPTIONS",
    "access-control-max-age": "600",
    ...(origin === "*" ? {} : { vary: "origin" }),
  };
}

function text(body, contentType, extra = {}) {
  return new Response(body, {
    headers: securityHeaders({
      "content-type": contentType,
      "content-security-policy": CSP,
      ...extra,
    }),
  });
}

export async function handle(request, env) {
  const url = new URL(request.url);
  const path = url.pathname.replace(/\/+$/, "") || "/";
  const cors = corsOrigin(request, env);

  if (request.method === "OPTIONS") {
    return new Response(null, {
      status: 204,
      headers: { ...securityHeaders(), ...corsHeaders(cors) },
    });
  }

  if (path === "/healthz") {
    return text("ok\n", "text/plain; charset=utf-8");
  }

  if (request.method === "GET") {
    if (path === "/") {
      return text(
        indexHtml({ sealed: isSealed(env), needsKey: !isPublic(env) }),
        "text/html; charset=utf-8",
        { "cache-control": "no-store" },
      );
    }
    if (path === "/app.css") {
      return text(APP_CSS, "text/css; charset=utf-8", {
        "cache-control": "public, max-age=300",
      });
    }
    if (path === "/app.js") {
      return text(APP_JS, "text/javascript; charset=utf-8", {
        "cache-control": "public, max-age=300",
      });
    }
    if (path === "/favicon.svg") {
      return text(FAVICON, "image/svg+xml", {
        "cache-control": "public, max-age=86400",
      });
    }
  }

  if (
    path === "/v1/models" ||
    path === "/v1/chat/completions" ||
    path === "/v1/diagnostics"
  ) {
    return api(request, env, path, cors);
  }

  return errorResponse(
    new RequestError("Not found", 404, "not_found"),
    { ...securityHeaders(), ...corsHeaders(cors) },
  );
}

async function api(request, env, path, cors) {
  const headers = { ...securityHeaders(), ...corsHeaders(cors) };

  const auth = authorize(request, env);
  if (!auth.ok) {
    return errorResponse(auth, {
      ...headers,
      // Tell an OpenAI client how to authenticate rather than leaving it to
      // guess between a bearer token and a header.
      ...(auth.status === 401
        ? { "www-authenticate": 'Bearer realm="telemt-assistant"' }
        : {}),
    });
  }

  const limited = throttle(throttleIdentity(request, auth), env);
  if (!limited.ok) {
    return errorResponse(limited, {
      ...headers,
      "retry-after": String(limited.retryAfter),
    });
  }

  if (path === "/v1/models") {
    if (request.method !== "GET") {
      return errorResponse(
        new RequestError("Use GET for /v1/models.", 405, "method_not_allowed"),
        headers,
      );
    }
    const response = modelsResponse();
    for (const [name, value] of Object.entries(headers)) {
      response.headers.set(name, value);
    }
    return response;
  }

  if (path === "/v1/diagnostics") {
    if (request.method !== "GET") {
      return errorResponse(
        new RequestError("Use GET for /v1/diagnostics.", 405, "method_not_allowed"),
        headers,
      );
    }
    const assistantKeys = String(env.ASSISTANT_API_KEYS || "")
      .split(",")
      .map((key) => key.trim())
      .filter(Boolean);
    const response = diagnosticsResponse({
      config: upstreamConfig(env),
      assistantKeys,
      access: {
        mode: isPublic(env) ? "public" : "api_key",
        configured_keys: assistantKeys.length,
        rate_limit: env.ASSISTANT_RATE_LIMIT || null,
      },
    });
    for (const [name, value] of Object.entries(headers)) {
      response.headers.set(name, value);
    }
    return response;
  }

  if (request.method !== "POST") {
    return errorResponse(
      new RequestError("Use POST for /v1/chat/completions.", 405, "method_not_allowed"),
      headers,
    );
  }

  const declared = Number.parseInt(request.headers.get("content-length") ?? "", 10);
  if (Number.isFinite(declared) && declared > MAX_BODY_BYTES) {
    return errorResponse(
      new RequestError("Request body is too large.", 413, "payload_too_large"),
      headers,
    );
  }

  let body;
  try {
    const raw = await request.text();
    if (raw.length > MAX_BODY_BYTES) {
      return errorResponse(
        new RequestError("Request body is too large.", 413, "payload_too_large"),
        headers,
      );
    }
    body = JSON.parse(raw);
  } catch {
    return errorResponse(new RequestError("Body must be valid JSON."), headers);
  }

  let normalised;
  try {
    normalised = normaliseRequest(body);
  } catch (error) {
    return errorResponse(
      error instanceof RequestError ? error : new RequestError("Invalid request."),
      headers,
    );
  }

  const config = upstreamConfig(env);

  if (normalised.stream) {
    // Streaming responses report failures in-band; the status line is written
    // before the first token, so it cannot carry a later error.
    const response = streamingResponse({ config, request: normalised });
    for (const [name, value] of Object.entries(headers)) {
      response.headers.set(name, value);
    }
    return response;
  }

  try {
    const response = await blockingResponse({ config, request: normalised });
    for (const [name, value] of Object.entries(headers)) {
      response.headers.set(name, value);
    }
    return response;
  } catch (error) {
    if (error instanceof UpstreamError || error instanceof RequestError) {
      return errorResponse(error, headers);
    }
    return errorResponse(
      new UpstreamError("The assistant failed to answer.", 502),
      headers,
    );
  }
}
