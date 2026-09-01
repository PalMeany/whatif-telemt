/**
 * Access control for the endpoint.
 *
 * This service spends the operator's model credit on every request, so it fails
 * closed: with no keys configured and no explicit opt-in to open access, it
 * serves nothing. An OpenAI-compatible endpoint left open is a faucet, and it
 * will be found.
 */

/** Compares two strings without leaking their common prefix through timing. */
function constantTimeEqual(a, b) {
  const left = new TextEncoder().encode(a);
  const right = new TextEncoder().encode(b);
  // Comparing different lengths would return early; fold the length in instead.
  let difference = left.length ^ right.length;
  const length = Math.max(left.length, right.length);
  for (let index = 0; index < length; index += 1) {
    difference |= (left[index] ?? 0) ^ (right[index] ?? 0);
  }
  return difference === 0;
}

function configuredKeys(env) {
  return String(env.ASSISTANT_API_KEYS || "")
    .split(",")
    .map((key) => key.trim())
    .filter(Boolean);
}

export function isPublic(env) {
  return /^(1|true|yes|on)$/i.test(String(env.ASSISTANT_PUBLIC || "").trim());
}

/** True when the deployment will answer nobody, so the site can say why. */
export function isSealed(env) {
  return !isPublic(env) && configuredKeys(env).length === 0;
}

function presentedKey(request) {
  const header = request.headers.get("authorization") || "";
  const bearer = header.match(/^Bearer\s+(.+)$/i);
  if (bearer) return bearer[1].trim();
  // What some OpenAI clients send instead.
  return (request.headers.get("x-api-key") || "").trim();
}

/**
 * Result is `{ ok: true, key }` or `{ ok: false, status, message, code }`.
 */
export function authorize(request, env) {
  if (isPublic(env)) return { ok: true, key: "public" };

  const keys = configuredKeys(env);
  if (keys.length === 0) {
    return {
      ok: false,
      status: 503,
      code: "not_configured",
      message:
        "This deployment has no API keys configured. Set ASSISTANT_API_KEYS, " +
        "or ASSISTANT_PUBLIC=1 to serve without authentication.",
    };
  }

  const presented = presentedKey(request);
  if (!presented) {
    return {
      ok: false,
      status: 401,
      code: "missing_api_key",
      message: "Provide an API key: Authorization: Bearer <key>.",
    };
  }
  // Every configured key is compared so the answer takes the same time
  // regardless of which one matched, or whether any did.
  let matched = false;
  for (const key of keys) {
    if (constantTimeEqual(key, presented)) matched = true;
  }
  if (!matched) {
    return {
      ok: false,
      status: 401,
      code: "invalid_api_key",
      message: "The API key is not valid.",
    };
  }
  return { ok: true, key: presented };
}

/**
 * Best-effort per-isolate request throttle.
 *
 * Serverless runtimes give no shared state, so this bounds one instance and no
 * more: it stops a single client hammering one warm isolate, and it is not a
 * substitute for the platform's own rate limiting. Say so rather than implying
 * a guarantee this cannot make — Cloudflare Rate Limiting rules, a Vercel
 * firewall rule, or an API Gateway quota in Yandex Cloud are the real control.
 */
const buckets = new Map();
const MAX_BUCKETS = 2048;

export function throttle(identity, env) {
  const limit = Number.parseInt(env.ASSISTANT_RATE_LIMIT ?? "", 10);
  if (!Number.isFinite(limit) || limit <= 0) return { ok: true };

  const windowMs =
    (Number.parseInt(env.ASSISTANT_RATE_WINDOW_SECS ?? "", 10) || 60) * 1000;
  const now = Date.now();

  if (buckets.size > MAX_BUCKETS) {
    for (const [key, bucket] of buckets) {
      if (bucket.resetAt <= now) buckets.delete(key);
    }
    // Still full: an isolate under a spray of distinct identities. Drop it all
    // rather than grow without bound; the window is a minute.
    if (buckets.size > MAX_BUCKETS) buckets.clear();
  }

  const bucket = buckets.get(identity);
  if (!bucket || bucket.resetAt <= now) {
    buckets.set(identity, { count: 1, resetAt: now + windowMs });
    return { ok: true };
  }
  if (bucket.count >= limit) {
    return {
      ok: false,
      status: 429,
      code: "rate_limit_exceeded",
      message: "Too many requests. Retry shortly.",
      retryAfter: Math.max(1, Math.ceil((bucket.resetAt - now) / 1000)),
    };
  }
  bucket.count += 1;
  return { ok: true };
}

/** Identity a throttle bucket is keyed on: the key, else the client address. */
export function throttleIdentity(request, auth) {
  if (auth.key && auth.key !== "public") return `key:${auth.key}`;
  const address =
    request.headers.get("cf-connecting-ip") ||
    request.headers.get("x-real-ip") ||
    (request.headers.get("x-forwarded-for") || "").split(",")[0].trim();
  return `ip:${address || "unknown"}`;
}
