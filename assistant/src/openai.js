import { systemPrompt } from "./knowledge.js";
import { streamCompletion, UpstreamError } from "./upstream.js";

/**
 * The OpenAI-compatible surface.
 *
 * Being OpenAI-shaped is the whole point: an operator can point any client they
 * already have — a chat UI, a shell alias, an editor plugin — at this endpoint
 * and get a model that knows telemt, without that client learning anything new.
 */

/** Model id this service advertises, independent of what the upstream runs. */
export const SERVED_MODEL = "telemt-assistant";

const MAX_MESSAGES = 60;
const MAX_CHARS = 400_000;

/** Refusal carrying the OpenAI error envelope. */
export class RequestError extends Error {
  constructor(message, status = 400, code = "invalid_request_error") {
    super(message);
    this.name = "RequestError";
    this.status = status;
    this.code = code;
  }
}

export function errorResponse(error, extraHeaders = {}) {
  const status = error?.status ?? 500;
  const body = {
    error: {
      message: error?.message ?? "Internal error",
      type: error?.code ?? "internal_error",
      param: null,
      code: error?.code ?? null,
    },
  };
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json; charset=utf-8", ...extraHeaders },
  });
}

/** `GET /v1/models`. */
export function modelsResponse() {
  return Response.json({
    object: "list",
    data: [
      {
        id: SERVED_MODEL,
        object: "model",
        created: 0,
        owned_by: "telemt-assistant",
      },
    ],
  });
}

/**
 * Flattens an OpenAI message into the shape the upstream wants.
 *
 * Content may be a string or the multipart array modern clients send. Only text
 * parts survive: this assistant reads configuration files, not images, and
 * silently dropping an image is friendlier than refusing the whole request.
 */
function flattenContent(content) {
  if (typeof content === "string") return content;
  if (!Array.isArray(content)) return "";
  return content
    .filter((part) => part && (part.type === "text" || part.type === "input_text"))
    .map((part) => part.text ?? "")
    .join("");
}

/**
 * Normalises a request body into `{ system, messages, maxTokens, stream }`.
 *
 * The caller's system turns are appended to this service's own prompt rather
 * than replacing it — the telemt knowledge is the product, and a client that
 * sends its own persona should refine it, not delete it.
 */
export function normaliseRequest(body) {
  if (!body || typeof body !== "object") {
    throw new RequestError("Request body must be a JSON object.");
  }
  const incoming = body.messages;
  if (!Array.isArray(incoming) || incoming.length === 0) {
    throw new RequestError("`messages` must be a non-empty array.");
  }
  if (incoming.length > MAX_MESSAGES) {
    throw new RequestError(
      `\`messages\` may contain at most ${MAX_MESSAGES} entries.`,
      413,
      "context_length_exceeded",
    );
  }

  const extraSystem = [];
  const messages = [];
  let characters = 0;

  for (const message of incoming) {
    if (!message || typeof message !== "object") {
      throw new RequestError("Every message must be an object.");
    }
    const role = message.role;
    const content = flattenContent(message.content);
    characters += content.length;
    if (role === "system" || role === "developer") {
      if (content) extraSystem.push(content);
      continue;
    }
    if (role !== "user" && role !== "assistant") {
      throw new RequestError(`Unsupported message role: ${role}`);
    }
    // The Messages API rejects an empty content block; an assistant turn with
    // nothing in it is what a client sends after an aborted stream.
    if (!content) continue;
    messages.push({ role, content });
  }

  if (characters > MAX_CHARS) {
    throw new RequestError(
      "The conversation is too large. Trim it and retry.",
      413,
      "context_length_exceeded",
    );
  }
  if (messages.length === 0) {
    throw new RequestError("At least one user message is required.");
  }
  if (messages[0].role !== "user") {
    throw new RequestError("The first message must be from the user.");
  }

  const system = extraSystem.length
    ? `${systemPrompt()}\n\n## Additional instructions from the caller\n\n${extraSystem.join("\n\n")}`
    : systemPrompt();

  const requested = Number.parseInt(body.max_tokens ?? body.max_completion_tokens, 10);

  return {
    system,
    messages,
    maxTokens: Number.isFinite(requested) && requested > 0 ? requested : 0,
    stream: body.stream === true,
  };
}

function completionId() {
  // Not a security value — it only has to be unique enough to correlate logs.
  return `chatcmpl-${crypto.randomUUID().replace(/-/g, "").slice(0, 24)}`;
}

function finishReason(stopReason) {
  switch (stopReason) {
    case "max_tokens":
      return "length";
    case "refusal":
      return "content_filter";
    default:
      return "stop";
  }
}

/** `POST /v1/chat/completions` with `stream: true`. */
export function streamingResponse({ config, request }) {
  const id = completionId();
  const created = Math.floor(Date.now() / 1000);
  const encoder = new TextEncoder();

  const chunk = (delta, reason = null) =>
    encoder.encode(
      `data: ${JSON.stringify({
        id,
        object: "chat.completion.chunk",
        created,
        model: SERVED_MODEL,
        choices: [{ index: 0, delta, finish_reason: reason }],
      })}\n\n`,
    );

  const body = new ReadableStream({
    async start(controller) {
      controller.enqueue(chunk({ role: "assistant", content: "" }));
      let reason = "stop";
      try {
        for await (const event of streamCompletion({
          config,
          system: request.system,
          messages: request.messages,
          maxTokens: request.maxTokens,
        })) {
          if (event.type === "text") {
            controller.enqueue(chunk({ content: event.text }));
          } else if (event.type === "thinking") {
            // The field OpenAI-compatible clients read reasoning from.
            controller.enqueue(chunk({ reasoning_content: event.text }));
          } else if (event.type === "done") {
            reason = finishReason(event.stopReason);
          }
        }
        controller.enqueue(chunk({}, reason));
      } catch (error) {
        // The status line is long gone, so a mid-stream failure has to be told
        // in-band or the client sees a truncated answer with no explanation.
        const message =
          error instanceof UpstreamError || error instanceof RequestError
            ? error.message
            : "The assistant failed mid-response.";
        controller.enqueue(chunk({ content: `\n\n[error] ${message}` }));
        controller.enqueue(chunk({}, "stop"));
      } finally {
        controller.enqueue(encoder.encode("data: [DONE]\n\n"));
        controller.close();
      }
    },
  });

  return new Response(body, {
    headers: {
      "content-type": "text/event-stream; charset=utf-8",
      "cache-control": "no-store",
      connection: "keep-alive",
      // Without this an nginx in front buffers the whole stream and the point
      // of streaming is lost.
      "x-accel-buffering": "no",
    },
  });
}

/** `POST /v1/chat/completions` without streaming. */
export async function blockingResponse({ config, request }) {
  let text = "";
  let reasoning = "";
  let stopReason = "end_turn";
  let usage = { input: 0, output: 0 };

  for await (const event of streamCompletion({
    config,
    system: request.system,
    messages: request.messages,
    maxTokens: request.maxTokens,
  })) {
    if (event.type === "text") text += event.text;
    else if (event.type === "thinking") reasoning += event.text;
    else if (event.type === "done") {
      stopReason = event.stopReason;
      usage = event.usage;
    }
  }

  return Response.json({
    id: completionId(),
    object: "chat.completion",
    created: Math.floor(Date.now() / 1000),
    model: SERVED_MODEL,
    choices: [
      {
        index: 0,
        message: {
          role: "assistant",
          content: text,
          ...(reasoning ? { reasoning_content: reasoning } : {}),
        },
        finish_reason: finishReason(stopReason),
      },
    ],
    usage: {
      prompt_tokens: usage.input,
      completion_tokens: usage.output,
      total_tokens: usage.input + usage.output,
    },
  });
}
