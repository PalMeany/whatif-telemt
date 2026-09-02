import Anthropic from "@anthropic-ai/sdk";

/**
 * The model call, behind one interface with two implementations.
 *
 * `anthropic` speaks the Messages API through the official SDK. Pointing
 * `UPSTREAM_BASE_URL` at a router that reimplements that API — the common shape
 * for a "Claude router" — needs no other change.
 *
 * `openai` speaks Chat Completions, for routers that only offer that shape
 * (OpenRouter and friends). It is a deliberate second path rather than the
 * default: the Messages API is where Claude's own features live, and a router
 * that flattens them costs thinking control and effort.
 *
 * Both yield the same event stream, so nothing above this file knows which one
 * answered:
 *   { type: "text", text }
 *   { type: "thinking", text }
 *   { type: "done", stopReason, usage: { input, output }, model }
 */

/** Answers Claude's own endpoint, where the beta parameters are understood. */
const FIRST_PARTY_HOSTS = new Set(["api.anthropic.com"]);

/** Reads the upstream configuration out of the environment. */
export function upstreamConfig(env) {
  const kind = (env.UPSTREAM_KIND || "anthropic").toLowerCase();
  const baseURL = env.UPSTREAM_BASE_URL || env.ANTHROPIC_BASE_URL || "";
  // Trimmed on purpose. `echo "sk-…" | wrangler secret put` stores the newline
  // too, and an upstream then rejects a key that looks correct everywhere the
  // operator can see it.
  const rawApiKey = env.UPSTREAM_API_KEY || env.ANTHROPIC_API_KEY || "";
  const apiKey = rawApiKey.trim();
  const model =
    env.UPSTREAM_MODEL ||
    (kind === "openai" ? "anthropic/claude-opus-5" : "claude-opus-5");

  let firstParty = baseURL === "";
  if (baseURL) {
    try {
      firstParty = FIRST_PARTY_HOSTS.has(new URL(baseURL).hostname);
    } catch {
      firstParty = false;
    }
  }

  return {
    kind,
    baseURL,
    apiKey,
    // Reported by diagnostics so a key mangled in transit is visible without
    // anyone having to print the key itself.
    apiKeyHadWhitespace: rawApiKey !== apiKey,
    model,
    firstParty,
    effort: env.UPSTREAM_EFFORT || "high",
    maxTokens: positiveInt(env.UPSTREAM_MAX_TOKENS, 16000),
    // Adaptive thinking is on by default on Opus 5. Streaming a summary of it
    // is opt-in because most OpenAI clients have nowhere to put it, and the
    // ones that do read `reasoning_content`.
    showThinking: truthy(env.UPSTREAM_SHOW_THINKING),
    // A refusal fallback is a first-party beta. A router will reject the
    // parameter outright, so it is only sent where it is understood — unless an
    // operator whose router does support it says otherwise.
    fallbacks: env.UPSTREAM_FALLBACKS
      ? truthy(env.UPSTREAM_FALLBACKS)
      : firstParty,
  };
}

function positiveInt(value, fallback) {
  const parsed = Number.parseInt(value ?? "", 10);
  return Number.isFinite(parsed) && parsed > 0 ? parsed : fallback;
}

function truthy(value) {
  return /^(1|true|yes|on)$/i.test(String(value ?? "").trim());
}

/** Thrown for anything the caller should see as an upstream failure. */
export class UpstreamError extends Error {
  constructor(message, status = 502, code = "upstream_error") {
    super(message);
    this.name = "UpstreamError";
    this.status = status;
    this.code = code;
  }
}

/**
 * Classifies a credential without revealing it.
 *
 * Enough to spot the mistake that produces the most confusing failure of all —
 * the two keys a deployment holds swapped round — while leaking nothing: an
 * upstream that rejects a key "because the format is wrong" is describing this
 * shape, and the shape is all anyone needs to see the mix-up.
 */
export function describeKeyShape(key) {
  if (!key) return "unset";
  if (/^sk-/.test(key)) return "sk-prefixed";
  if (/^[A-Za-z0-9+/]{16,}={0,2}$/.test(key)) return "base64-like";
  if (/^[0-9a-f]{16,}$/i.test(key)) return "hex-like";
  return "other";
}

/**
 * A one-line description of where a request was sent, safe to show a caller.
 *
 * Every upstream failure carries it. Without it, a deployment whose variables
 * never arrived reports "the upstream rejected the credential" while quietly
 * talking to a completely different upstream than the operator configured —
 * which is indistinguishable from a wrong key until you go looking.
 */
export function describeUpstream(config) {
  const target = config.baseURL || "api.anthropic.com (default)";
  return `${config.kind} at ${target}, model ${config.model}`;
}

/**
 * Streams one completion.
 *
 * `messages` is already normalised to `{role, content}` with a string content
 * and no system turns — the caller lifts those into `system`.
 */
export async function* streamCompletion({ config, system, messages, maxTokens }) {
  if (!config.apiKey) {
    throw new UpstreamError(
      `No upstream credential is configured (${describeUpstream(config)}). ` +
        "Set UPSTREAM_API_KEY.",
      500,
      "missing_upstream_key",
    );
  }
  const budget = maxTokens || config.maxTokens;
  if (config.kind === "openai") {
    yield* streamOpenAiUpstream({ config, system, messages, budget });
  } else {
    yield* streamAnthropicUpstream({ config, system, messages, budget });
  }
}

/** The Messages API, through the official SDK. */
async function* streamAnthropicUpstream({ config, system, messages, budget }) {
  const client = new Anthropic({
    apiKey: config.apiKey,
    ...(config.baseURL ? { baseURL: config.baseURL } : {}),
  });

  const request = {
    model: config.model,
    max_tokens: budget,
    // A frozen prefix: the system prompt never varies between requests, so it
    // is worth a cache breakpoint of its own.
    system: [
      { type: "text", text: system, cache_control: { type: "ephemeral" } },
    ],
    messages,
    output_config: { effort: config.effort },
    thinking: config.showThinking
      ? { type: "adaptive", display: "summarized" }
      : { type: "adaptive" },
  };

  let stream;
  try {
    stream = config.fallbacks
      ? client.beta.messages.stream({
          ...request,
          betas: ["server-side-fallback-2026-07-01"],
          fallbacks: "default",
        })
      : client.messages.stream(request);
  } catch (error) {
    throw translateAnthropicError(error, config);
  }

  try {
    for await (const event of stream) {
      if (event.type !== "content_block_delta") continue;
      if (event.delta.type === "text_delta") {
        yield { type: "text", text: event.delta.text };
      } else if (event.delta.type === "thinking_delta" && config.showThinking) {
        yield { type: "thinking", text: event.delta.thinking };
      }
    }
    const final = await stream.finalMessage();
    // A refusal is an HTTP 200 with no useful content, so it has to be turned
    // into something the caller can render rather than an empty answer.
    if (final.stop_reason === "refusal") {
      yield {
        type: "text",
        text:
          "\n\n_The model declined this request" +
          (final.stop_details?.category
            ? ` (${final.stop_details.category})`
            : "") +
          "._",
      };
    }
    yield {
      type: "done",
      stopReason: final.stop_reason,
      model: final.model,
      usage: {
        input: final.usage?.input_tokens ?? 0,
        output: final.usage?.output_tokens ?? 0,
        cacheRead: final.usage?.cache_read_input_tokens ?? 0,
      },
    };
  } catch (error) {
    throw translateAnthropicError(error, config);
  }
}

/** Maps the SDK's typed errors onto what the caller should be told. */
function translateAnthropicError(error, config) {
  if (error instanceof UpstreamError) return error;
  const where = describeUpstream(config);
  if (error instanceof Anthropic.AuthenticationError) {
    return new UpstreamError(
      `The upstream rejected the credential (${where}).`,
      502,
      "upstream_auth_failed",
    );
  }
  if (error instanceof Anthropic.RateLimitError) {
    return new UpstreamError(
      `The upstream is rate limiting (${where}). Retry shortly.`,
      429,
      "upstream_rate_limited",
    );
  }
  if (error instanceof Anthropic.BadRequestError) {
    // Usually a router that does not understand a parameter this code sent.
    return new UpstreamError(
      `The upstream refused the request (${where}): ${error.message}`,
      502,
      "upstream_bad_request",
    );
  }
  if (error instanceof Anthropic.APIError) {
    return new UpstreamError(
      `Upstream error ${error.status} (${where}): ${error.message}`,
      502,
      "upstream_error",
    );
  }
  return new UpstreamError(
    `${error?.message || "The upstream could not be reached"} (${where}).`,
    502,
    "upstream_unreachable",
  );
}

/**
 * Chat Completions, for a router that speaks only that shape.
 *
 * Raw fetch rather than a second SDK: this is one request and one SSE parse,
 * and the wire format is the same one this service already exposes.
 */
async function* streamOpenAiUpstream({ config, system, messages, budget }) {
  const base = (config.baseURL || "https://openrouter.ai/api").replace(
    /\/+$/,
    "",
  );
  const url = base.endsWith("/v1")
    ? `${base}/chat/completions`
    : `${base}/v1/chat/completions`;

  let response;
  try {
    response = await fetch(url, {
      method: "POST",
      headers: {
        "content-type": "application/json",
        authorization: `Bearer ${config.apiKey}`,
      },
      body: JSON.stringify({
        model: config.model,
        max_tokens: budget,
        stream: true,
        messages: [{ role: "system", content: system }, ...messages],
      }),
    });
  } catch (error) {
    throw new UpstreamError(
      `The upstream could not be reached (${describeUpstream(config)}): ` +
        `${error?.message ?? "network error"}`,
      502,
      "upstream_unreachable",
    );
  }

  if (!response.ok || !response.body) {
    const detail = (await response.text().catch(() => "")).slice(0, 400);
    throw new UpstreamError(
      `Upstream error ${response.status} (${describeUpstream(config)})` +
        `${detail ? `: ${detail}` : ""}`,
      response.status === 429 ? 429 : 502,
      response.status === 429 ? "upstream_rate_limited" : "upstream_error",
    );
  }

  let stopReason = "stop";
  let model = config.model;
  let usage = { input: 0, output: 0, cacheRead: 0 };

  for await (const payload of sseData(response.body)) {
    if (payload === "[DONE]") break;
    let chunk;
    try {
      chunk = JSON.parse(payload);
    } catch {
      continue;
    }
    if (chunk.model) model = chunk.model;
    if (chunk.usage) {
      usage = {
        input: chunk.usage.prompt_tokens ?? 0,
        output: chunk.usage.completion_tokens ?? 0,
        cacheRead: 0,
      };
    }
    const choice = chunk.choices?.[0];
    if (!choice) continue;
    if (choice.finish_reason) stopReason = choice.finish_reason;
    const delta = choice.delta ?? {};
    // Routers that surface reasoning use one of these two field names.
    const reasoning = delta.reasoning_content ?? delta.reasoning;
    if (typeof reasoning === "string" && reasoning && config.showThinking) {
      yield { type: "thinking", text: reasoning };
    }
    if (typeof delta.content === "string" && delta.content) {
      yield { type: "text", text: delta.content };
    }
  }

  yield { type: "done", stopReason, model, usage };
}

/** Yields the `data:` payload of each SSE event in a byte stream. */
async function* sseData(body) {
  const reader = body.getReader();
  const decoder = new TextDecoder();
  let buffer = "";
  try {
    while (true) {
      const { done, value } = await reader.read();
      if (done) break;
      buffer += decoder.decode(value, { stream: true });
      let boundary;
      // Events are separated by a blank line; a chunk may split one in half.
      while ((boundary = buffer.indexOf("\n\n")) !== -1) {
        const raw = buffer.slice(0, boundary);
        buffer = buffer.slice(boundary + 2);
        const data = raw
          .split("\n")
          .filter((line) => line.startsWith("data:"))
          .map((line) => line.slice(5).trim())
          .join("");
        if (data) yield data;
      }
    }
  } finally {
    reader.releaseLock();
  }
}
