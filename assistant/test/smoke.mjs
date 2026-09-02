// End-to-end checks that need no credential and no network beyond loopback.
//
// A fake OpenAI-shaped router stands in for the upstream, so the whole path is
// exercised: auth, the request normaliser, the SSE parser, the response
// shaping, and the Yandex adapter's event translation.

import http from "node:http";
import assert from "node:assert/strict";
import { handle } from "../src/app.js";
import { handler as yandexHandler } from "../adapters/yandex.js";

let failures = 0;
let passed = 0;

async function check(name, fn) {
  try {
    await fn();
    passed += 1;
  } catch (error) {
    failures += 1;
    console.error(`FAIL  ${name}\n      ${error.message}`);
  }
}

/** A router that answers Chat Completions with two SSE chunks. */
function startFakeRouter() {
  const seen = [];
  const server = http.createServer((req, res) => {
    let body = "";
    req.on("data", (chunk) => (body += chunk));
    req.on("end", () => {
      seen.push({ url: req.url, auth: req.headers.authorization, body: JSON.parse(body) });
      res.writeHead(200, { "content-type": "text/event-stream" });
      const chunk = (delta, finish = null) =>
        `data: ${JSON.stringify({
          model: "router/claude-opus-5",
          choices: [{ index: 0, delta, finish_reason: finish }],
        })}\n\n`;
      res.write(chunk({ role: "assistant" }));
      res.write(chunk({ content: "Set " }));
      // Split across writes so the parser has to stitch a partial event.
      res.write("data: " + JSON.stringify({
        choices: [{ index: 0, delta: { content: "`panel.enabled`" }, finish_reason: null }],
      }));
      res.write("\n\n" + chunk({}, "stop"));
      res.write("data: [DONE]\n\n");
      res.end();
    });
  });
  return new Promise((resolve) => {
    server.listen(0, "127.0.0.1", () =>
      resolve({ server, port: server.address().port, seen }),
    );
  });
}

const { server, port, seen } = await startFakeRouter();

const env = {
  ASSISTANT_API_KEYS: "test-key-one,test-key-two",
  UPSTREAM_KIND: "openai",
  UPSTREAM_BASE_URL: `http://127.0.0.1:${port}`,
  UPSTREAM_API_KEY: "router-secret",
  UPSTREAM_MODEL: "router/claude-opus-5",
};

const post = (body, headers = {}) =>
  handle(
    new Request("https://assistant.example/v1/chat/completions", {
      method: "POST",
      headers: { "content-type": "application/json", ...headers },
      body: JSON.stringify(body),
    }),
    env,
  );

const authed = { authorization: "Bearer test-key-one" };
const ask = { model: "telemt-assistant", messages: [{ role: "user", content: "hi" }] };

await check("the site is served with a strict policy and no inline exception", async () => {
  const response = await handle(new Request("https://assistant.example/"), env);
  assert.equal(response.status, 200);
  const csp = response.headers.get("content-security-policy");
  assert.match(csp, /script-src 'self'/);
  assert.ok(!csp.includes("unsafe-inline"), "no inline exception");
  assert.equal(response.headers.get("x-frame-options"), "DENY");
  const html = await response.text();
  assert.ok(!/<script>[^<]/.test(html), "no inline script in the page");
  assert.match(html, /app\.js/);
});

await check("the health probe needs no credential", async () => {
  const response = await handle(new Request("https://assistant.example/healthz"), env);
  assert.equal(response.status, 200);
  assert.equal((await response.text()).trim(), "ok");
});

await check("a deployment with no keys refuses to answer", async () => {
  const response = await handle(
    new Request("https://assistant.example/v1/models"),
    { ...env, ASSISTANT_API_KEYS: "" },
  );
  assert.equal(response.status, 503);
  assert.equal((await response.json()).error.code, "not_configured");
});

await check("a missing key is 401 with a challenge", async () => {
  const response = await handle(new Request("https://assistant.example/v1/models"), env);
  assert.equal(response.status, 401);
  assert.match(response.headers.get("www-authenticate") ?? "", /Bearer/);
});

await check("a wrong key is refused", async () => {
  const response = await handle(
    new Request("https://assistant.example/v1/models", {
      headers: { authorization: "Bearer nope" },
    }),
    env,
  );
  assert.equal(response.status, 401);
  assert.equal((await response.json()).error.code, "invalid_api_key");
});

await check("every configured key is accepted", async () => {
  for (const key of ["test-key-one", "test-key-two"]) {
    const response = await handle(
      new Request("https://assistant.example/v1/models", {
        headers: { authorization: `Bearer ${key}` },
      }),
      env,
    );
    assert.equal(response.status, 200, key);
    assert.equal((await response.json()).data[0].id, "telemt-assistant");
  }
});

await check("public mode needs no key", async () => {
  const response = await handle(
    new Request("https://assistant.example/v1/models"),
    { ...env, ASSISTANT_API_KEYS: "", ASSISTANT_PUBLIC: "1" },
  );
  assert.equal(response.status, 200);
});

await check("a non-streaming completion carries the OpenAI shape", async () => {
  const response = await post(ask, authed);
  assert.equal(response.status, 200);
  const payload = await response.json();
  assert.equal(payload.object, "chat.completion");
  assert.equal(payload.model, "telemt-assistant");
  assert.equal(payload.choices[0].finish_reason, "stop");
  assert.equal(payload.choices[0].message.role, "assistant");
  assert.equal(payload.choices[0].message.content, "Set `panel.enabled`");
});

await check("the upstream receives the system prompt and the credential", async () => {
  seen.length = 0;
  await post(ask, authed);
  const call = seen.at(-1);
  assert.equal(call.auth, "Bearer router-secret");
  assert.match(call.url, /\/v1\/chat\/completions$/);
  assert.equal(call.body.model, "router/claude-opus-5");
  assert.equal(call.body.messages[0].role, "system");
  assert.match(call.body.messages[0].content, /telemt assistant/);
  // The grounding matters more than anything else in that prompt.
  assert.match(call.body.messages[0].content, /Never invent a configuration key/);
  assert.match(call.body.messages[0].content, /\[panel\.cluster\]: enabled, role/);
});

await check("a caller's system turn refines the prompt rather than replacing it", async () => {
  seen.length = 0;
  await post(
    {
      ...ask,
      messages: [
        { role: "system", content: "Answer in one sentence." },
        { role: "user", content: "hi" },
      ],
    },
    authed,
  );
  const system = seen.at(-1).body.messages[0].content;
  assert.match(system, /telemt assistant/);
  assert.match(system, /Additional instructions from the caller/);
  assert.match(system, /Answer in one sentence\./);
});

await check("a streaming completion emits OpenAI chunks and terminates", async () => {
  const response = await post({ ...ask, stream: true }, authed);
  assert.equal(response.status, 200);
  assert.match(response.headers.get("content-type"), /text\/event-stream/);
  const body = await response.text();
  assert.ok(body.endsWith("data: [DONE]\n\n"), "stream terminates with [DONE]");
  const deltas = body
    .split("\n\n")
    .filter((line) => line.startsWith("data: ") && !line.includes("[DONE]"))
    .map((line) => JSON.parse(line.slice(6)));
  assert.equal(deltas[0].choices[0].delta.role, "assistant");
  const text = deltas.map((d) => d.choices[0].delta.content ?? "").join("");
  assert.equal(text, "Set `panel.enabled`");
  assert.equal(deltas.at(-1).choices[0].finish_reason, "stop");
});

await check("a request with no messages is refused", async () => {
  const response = await post({ model: "telemt-assistant", messages: [] }, authed);
  assert.equal(response.status, 400);
  assert.match((await response.json()).error.message, /non-empty/);
});

await check("a conversation that starts with the assistant is refused", async () => {
  const response = await post(
    { messages: [{ role: "assistant", content: "hello" }] },
    authed,
  );
  assert.equal(response.status, 400);
});

await check("multipart text content is flattened", async () => {
  seen.length = 0;
  await post(
    {
      messages: [
        {
          role: "user",
          content: [
            { type: "text", text: "check " },
            { type: "image_url", image_url: { url: "https://example/x.png" } },
            { type: "text", text: "this config" },
          ],
        },
      ],
    },
    authed,
  );
  const messages = seen.at(-1).body.messages;
  assert.equal(messages.at(-1).content, "check this config");
});

await check("a malformed body is refused before the upstream is called", async () => {
  seen.length = 0;
  const response = await handle(
    new Request("https://assistant.example/v1/chat/completions", {
      method: "POST",
      headers: { "content-type": "application/json", ...authed },
      body: "{not json",
    }),
    env,
  );
  assert.equal(response.status, 400);
  assert.equal(seen.length, 0, "nothing reached the upstream");
});

await check("the wrong method is refused", async () => {
  const response = await handle(
    new Request("https://assistant.example/v1/chat/completions", {
      method: "GET",
      headers: authed,
    }),
    env,
  );
  assert.equal(response.status, 405);
});

await check("an unknown path is a 404 in the OpenAI envelope", async () => {
  const response = await handle(new Request("https://assistant.example/nope"), env);
  assert.equal(response.status, 404);
  assert.equal((await response.json()).error.type, "not_found");
});

await check("cross-origin is closed unless an origin is allowed", async () => {
  const closed = await handle(
    new Request("https://assistant.example/v1/models", {
      headers: { origin: "https://elsewhere.example", ...authed },
    }),
    env,
  );
  assert.equal(closed.headers.get("access-control-allow-origin"), null);

  const opened = await handle(
    new Request("https://assistant.example/v1/models", {
      headers: { origin: "https://elsewhere.example", ...authed },
    }),
    { ...env, ASSISTANT_CORS_ORIGINS: "https://elsewhere.example" },
  );
  assert.equal(
    opened.headers.get("access-control-allow-origin"),
    "https://elsewhere.example",
  );
});

await check("the throttle refuses past its ceiling", async () => {
  const limited = { ...env, ASSISTANT_RATE_LIMIT: "2", ASSISTANT_RATE_WINDOW_SECS: "60" };
  const call = () =>
    handle(
      new Request("https://assistant.example/v1/models", {
        headers: { authorization: "Bearer throttle-probe" },
      }),
      { ...limited, ASSISTANT_API_KEYS: "throttle-probe" },
    );
  assert.equal((await call()).status, 200);
  assert.equal((await call()).status, 200);
  const third = await call();
  assert.equal(third.status, 429);
  assert.ok(Number(third.headers.get("retry-after")) > 0);
});

await check("an upstream failure becomes a 502, not a crash", async () => {
  const response = await post(ask, {
    ...authed,
  });
  assert.equal(response.status, 200); // sanity: the good path still works
  const broken = await handle(
    new Request("https://assistant.example/v1/chat/completions", {
      method: "POST",
      headers: { "content-type": "application/json", ...authed },
      body: JSON.stringify(ask),
    }),
    { ...env, UPSTREAM_BASE_URL: "http://127.0.0.1:1" },
  );
  assert.equal(broken.status, 502);
  assert.match((await broken.json()).error.message, /could not be reached|Upstream/);
});

await check("a missing upstream credential is reported, not swallowed", async () => {
  const response = await handle(
    new Request("https://assistant.example/v1/chat/completions", {
      method: "POST",
      headers: { "content-type": "application/json", ...authed },
      body: JSON.stringify(ask),
    }),
    { ...env, UPSTREAM_API_KEY: "" },
  );
  assert.equal(response.status, 500);
  assert.equal((await response.json()).error.code, "missing_upstream_key");
});

await check("the Yandex adapter round-trips an event", async () => {
  const previous = { ...process.env };
  Object.assign(process.env, env);
  try {
    const result = await yandexHandler({
      httpMethod: "POST",
      url: "/v1/chat/completions",
      headers: { "content-type": "application/json", authorization: "Bearer test-key-one" },
      body: JSON.stringify(ask),
      isBase64Encoded: false,
    });
    assert.equal(result.statusCode, 200);
    assert.equal(result.isBase64Encoded, false);
    const payload = JSON.parse(result.body);
    assert.equal(payload.choices[0].message.content, "Set `panel.enabled`");

    const page = await yandexHandler({
      httpMethod: "GET",
      url: "/",
      headers: { host: "assistant.example" },
    });
    assert.equal(page.statusCode, 200);
    assert.match(page.headers["content-type"], /text\/html/);

    // A base64 body is what the gateway sends for anything it treats as binary.
    const encoded = await yandexHandler({
      httpMethod: "POST",
      url: "/v1/chat/completions",
      headers: { "content-type": "application/json", authorization: "Bearer test-key-one" },
      body: Buffer.from(JSON.stringify(ask)).toString("base64"),
      isBase64Encoded: true,
    });
    assert.equal(encoded.statusCode, 200);

    const streamed = await yandexHandler({
      httpMethod: "POST",
      url: "/v1/chat/completions",
      headers: { "content-type": "application/json", authorization: "Bearer test-key-one" },
      body: JSON.stringify({ ...ask, stream: true }),
    });
    assert.equal(streamed.statusCode, 200);
    // Buffered, and the header says so rather than pretending otherwise.
    assert.equal(streamed.headers["x-buffered-stream"], "yandex-cloud-functions");
    assert.ok(streamed.body.endsWith("data: [DONE]\n\n"));
  } finally {
    for (const name of Object.keys(env)) delete process.env[name];
    Object.assign(process.env, previous);
  }
});

/** A router that speaks the Anthropic Messages API instead. */
function startFakeAnthropic() {
  const seen = [];
  const server = http.createServer((req, res) => {
    let body = "";
    req.on("data", (chunk) => (body += chunk));
    req.on("end", () => {
      seen.push({
        url: req.url,
        key: req.headers["x-api-key"],
        beta: req.headers["anthropic-beta"],
        body: JSON.parse(body),
      });
      res.writeHead(200, { "content-type": "text/event-stream" });
      const event = (type, data) =>
        `event: ${type}\ndata: ${JSON.stringify({ type, ...data })}\n\n`;
      res.write(
        event("message_start", {
          message: {
            id: "msg_test",
            type: "message",
            role: "assistant",
            model: "claude-opus-5",
            content: [],
            stop_reason: null,
            stop_sequence: null,
            usage: { input_tokens: 11, output_tokens: 0 },
          },
        }),
      );
      res.write(
        event("content_block_start", {
          index: 0,
          content_block: { type: "text", text: "" },
        }),
      );
      res.write(
        event("content_block_delta", {
          index: 0,
          delta: { type: "text_delta", text: "Use " },
        }),
      );
      res.write(
        event("content_block_delta", {
          index: 0,
          delta: { type: "text_delta", text: "[panel.cluster]" },
        }),
      );
      res.write(event("content_block_stop", { index: 0 }));
      res.write(
        event("message_delta", {
          delta: { stop_reason: "end_turn", stop_sequence: null },
          usage: { output_tokens: 7 },
        }),
      );
      res.write(event("message_stop", {}));
      res.end();
    });
  });
  return new Promise((resolve) => {
    server.listen(0, "127.0.0.1", () =>
      resolve({ server, port: server.address().port, seen }),
    );
  });
}

const anthropic = await startFakeAnthropic();
const anthropicEnv = {
  ASSISTANT_API_KEYS: "test-key-one",
  UPSTREAM_KIND: "anthropic",
  UPSTREAM_BASE_URL: `http://127.0.0.1:${anthropic.port}`,
  UPSTREAM_API_KEY: "anthropic-secret",
};

await check("the Messages API path streams and shapes an answer", async () => {
  const response = await handle(
    new Request("https://assistant.example/v1/chat/completions", {
      method: "POST",
      headers: { "content-type": "application/json", ...authed },
      body: JSON.stringify(ask),
    }),
    anthropicEnv,
  );
  assert.equal(response.status, 200);
  const payload = await response.json();
  assert.equal(payload.choices[0].message.content, "Use [panel.cluster]");
  assert.equal(payload.usage.prompt_tokens, 11);
  assert.equal(payload.usage.completion_tokens, 7);
});

await check("the Messages request carries the model, cache breakpoint and effort", async () => {
  anthropic.seen.length = 0;
  await handle(
    new Request("https://assistant.example/v1/chat/completions", {
      method: "POST",
      headers: { "content-type": "application/json", ...authed },
      body: JSON.stringify(ask),
    }),
    anthropicEnv,
  );
  const call = anthropic.seen.at(-1);
  assert.equal(call.key, "anthropic-secret");
  assert.match(call.url, /\/v1\/messages$/);
  assert.equal(call.body.model, "claude-opus-5");
  assert.equal(call.body.stream, true);
  assert.equal(call.body.thinking.type, "adaptive");
  assert.equal(call.body.output_config.effort, "high");
  // The frozen prompt is its own cached block; the volatile turn is not.
  assert.equal(call.body.system[0].cache_control.type, "ephemeral");
  assert.match(call.body.system[0].text, /Never invent a configuration key/);
  assert.equal(call.body.messages[0].role, "user");
  // A refusal fallback is first-party only; a router would reject the beta.
  assert.equal(call.beta, undefined);
});

await check("a first-party upstream opts into refusal fallbacks", async () => {
  const { upstreamConfig } = await import("../src/upstream.js");
  assert.equal(upstreamConfig({ UPSTREAM_API_KEY: "k" }).fallbacks, true);
  assert.equal(
    upstreamConfig({
      UPSTREAM_API_KEY: "k",
      UPSTREAM_BASE_URL: "https://router.example.com",
    }).fallbacks,
    false,
  );
  // An operator whose router does support it can say so.
  assert.equal(
    upstreamConfig({
      UPSTREAM_API_KEY: "k",
      UPSTREAM_BASE_URL: "https://router.example.com",
      UPSTREAM_FALLBACKS: "true",
    }).fallbacks,
    true,
  );
});

await check("the model default follows the upstream shape", async () => {
  const { upstreamConfig } = await import("../src/upstream.js");
  assert.equal(upstreamConfig({}).model, "claude-opus-5");
  assert.equal(upstreamConfig({ UPSTREAM_KIND: "openai" }).model, "anthropic/claude-opus-5");
  assert.equal(upstreamConfig({ UPSTREAM_MODEL: "custom" }).model, "custom");
});

/** A router that rejects the credential, the way a wrong key looks. */
function startRejectingUpstream(shape) {
  const server = http.createServer((req, res) => {
    let body = "";
    req.on("data", (c) => (body += c));
    req.on("end", () => {
      res.writeHead(401, { "content-type": "application/json" });
      res.end(
        JSON.stringify(
          shape === "anthropic"
            ? { type: "error", error: { type: "authentication_error", message: "invalid x-api-key" } }
            : { error: { message: "Incorrect API key provided", type: "invalid_request_error" } },
        ),
      );
    });
  });
  return new Promise((resolve) => {
    server.listen(0, "127.0.0.1", () =>
      resolve({ server, port: server.address().port }),
    );
  });
}

await check("an upstream refusal names the upstream it actually used", async () => {
  // The failure an operator hits when the deployment's variables never arrived:
  // the message has to distinguish "wrong key" from "wrong upstream entirely".
  const rejecting = await startRejectingUpstream("anthropic");
  try {
    const response = await handle(
      new Request("https://assistant.example/v1/chat/completions", {
        method: "POST",
        headers: { "content-type": "application/json", ...authed },
        body: JSON.stringify(ask),
      }),
      {
        ASSISTANT_API_KEYS: "test-key-one",
        UPSTREAM_API_KEY: "a-router-key-sent-to-the-wrong-place",
        UPSTREAM_BASE_URL: `http://127.0.0.1:${rejecting.port}`,
      },
    );
    assert.equal(response.status, 502);
    const message = (await response.json()).error.message;
    assert.match(message, /rejected the credential/);
    // The three facts that turn the symptom into a cause.
    assert.match(message, /anthropic at/);
    assert.match(message, new RegExp(`127\\.0\\.0\\.1:${rejecting.port}`));
    assert.match(message, /model claude-opus-5/);
  } finally {
    rejecting.server.close();
  }
});

await check("the OpenAI path also names its upstream on failure", async () => {
  const rejecting = await startRejectingUpstream("openai");
  try {
    const response = await handle(
      new Request("https://assistant.example/v1/chat/completions", {
        method: "POST",
        headers: { "content-type": "application/json", ...authed },
        body: JSON.stringify(ask),
      }),
      {
        ASSISTANT_API_KEYS: "test-key-one",
        UPSTREAM_KIND: "openai",
        UPSTREAM_API_KEY: "wrong",
        UPSTREAM_BASE_URL: `http://127.0.0.1:${rejecting.port}`,
        UPSTREAM_MODEL: "anthropic/claude-opus-5",
      },
    );
    assert.equal(response.status, 502);
    const message = (await response.json()).error.message;
    assert.match(message, /Upstream error 401/);
    assert.match(message, /openai at/);
    assert.match(message, /model anthropic\/claude-opus-5/);
  } finally {
    rejecting.server.close();
  }
});

await check("diagnostics report what the deployment resolved", async () => {
  const response = await handle(
    new Request("https://assistant.example/v1/diagnostics", { headers: authed }),
    env,
  );
  assert.equal(response.status, 200);
  const payload = await response.json();
  assert.equal(payload.upstream.kind, "openai");
  assert.equal(payload.upstream.model, "router/claude-opus-5");
  assert.equal(payload.upstream.api_key_set, true);
  // The key itself must never appear, only its shape.
  assert.equal(JSON.stringify(payload).includes("router-secret"), false);
  assert.equal(payload.upstream.api_key_length, "router-secret".length);
  assert.equal(payload.access.mode, "api_key");
  assert.equal(payload.access.configured_keys, 2);
  assert.equal(payload.hint, null);
});

await check("diagnostics call out variables that never arrived", async () => {
  // Exactly the misconfiguration that produces a confusing 502: a router key
  // set, but UPSTREAM_KIND and UPSTREAM_BASE_URL missing from the deployment.
  const response = await handle(
    new Request("https://assistant.example/v1/diagnostics", { headers: authed }),
    { ASSISTANT_API_KEYS: "test-key-one", UPSTREAM_API_KEY: "router-key" },
  );
  const payload = await response.json();
  assert.equal(payload.upstream.resolved_target, "https://api.anthropic.com");
  assert.match(payload.hint, /did not reach this deployment/);
  assert.match(payload.hint, /\.dev\.vars` is local only/);
});

await check("diagnostics need the same credential as everything else", async () => {
  const response = await handle(
    new Request("https://assistant.example/v1/diagnostics"),
    env,
  );
  assert.equal(response.status, 401);
});

await check("swapped secrets are named outright", async () => {
  // The failure an operator cannot read from the upstream's own words: the
  // router says "that key format is wrong" and means the *other* key.
  const assistantKey = "5sgZrjS6Q/cFd94v+mzofna3G+x3kS1wD3JdrOf4ZBs=";
  const response = await handle(
    new Request("https://assistant.example/v1/diagnostics", {
      headers: { authorization: `Bearer ${assistantKey}` },
    }),
    {
      ASSISTANT_API_KEYS: assistantKey,
      UPSTREAM_KIND: "openai",
      UPSTREAM_BASE_URL: "https://api.example.com/v1",
      UPSTREAM_API_KEY: assistantKey,
    },
  );
  const payload = await response.json();
  assert.equal(payload.upstream.api_key_shape, "base64-like");
  assert.match(payload.hint, /byte-for-byte one of ASSISTANT_API_KEYS/);
  assert.equal(JSON.stringify(payload).includes(assistantKey), false);
});

await check("a key of the same shape as the deployment's own is flagged", async () => {
  const response = await handle(
    new Request("https://assistant.example/v1/diagnostics", {
      headers: { authorization: "Bearer AAAAAAAAAAAAAAAAAAAAAAAA" },
    }),
    {
      ASSISTANT_API_KEYS: "AAAAAAAAAAAAAAAAAAAAAAAA",
      UPSTREAM_KIND: "openai",
      UPSTREAM_BASE_URL: "https://api.example.com/v1",
      UPSTREAM_API_KEY: "BBBBBBBBBBBBBBBBBBBBBBBB",
    },
  );
  const payload = await response.json();
  assert.match(payload.hint, /same shape \(base64-like\)/);
});

await check("a correctly-shaped router key raises nothing", async () => {
  const response = await handle(
    new Request("https://assistant.example/v1/diagnostics", {
      headers: { authorization: "Bearer 5sgZrjS6Q/cFd94v+mzofna3G+x3kS1wD3JdrOf4ZBs=" },
    }),
    {
      ASSISTANT_API_KEYS: "5sgZrjS6Q/cFd94v+mzofna3G+x3kS1wD3JdrOf4ZBs=",
      UPSTREAM_KIND: "openai",
      UPSTREAM_BASE_URL: "https://api.claudy.shop/v1",
      UPSTREAM_API_KEY: "sk-a-real-router-key-value",
      UPSTREAM_MODEL: "claude-opus-5",
    },
  );
  const payload = await response.json();
  assert.equal(payload.upstream.api_key_shape, "sk-prefixed");
  assert.equal(payload.upstream.resolved_target, "https://api.claudy.shop/v1");
  assert.equal(payload.hint, null);
});

await check("a secret stored with a trailing newline is trimmed and reported", async () => {
  const response = await handle(
    new Request("https://assistant.example/v1/diagnostics", { headers: authed }),
    {
      ASSISTANT_API_KEYS: "test-key-one",
      UPSTREAM_KIND: "openai",
      UPSTREAM_BASE_URL: "https://api.example.com/v1",
      UPSTREAM_API_KEY: "sk-router-key\n",
    },
  );
  const payload = await response.json();
  assert.equal(payload.upstream.api_key_had_whitespace, true);
  assert.equal(payload.upstream.api_key_length, "sk-router-key".length);
  assert.match(payload.hint, /leading or trailing whitespace/);
});

await check("a trimmed key is what actually reaches the upstream", async () => {
  seen.length = 0;
  await handle(
    new Request("https://assistant.example/v1/chat/completions", {
      method: "POST",
      headers: { "content-type": "application/json", ...authed },
      body: JSON.stringify(ask),
    }),
    { ...env, UPSTREAM_API_KEY: "  router-secret\n" },
  );
  assert.equal(seen.at(-1).auth, "Bearer router-secret");
});

anthropic.server.close();
server.close();

console.log(`${passed} passed, ${failures} failed`);
process.exit(failures === 0 ? 0 : 1);
