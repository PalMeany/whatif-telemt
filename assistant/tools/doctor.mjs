// Checks the upstream before you deploy, so a wrong base URL or model id fails
// here with a readable message instead of as a 502 from a live Worker.
//
//   node tools/doctor.mjs            # reads .dev.vars, then the environment
//   UPSTREAM_BASE_URL=… node tools/doctor.mjs

import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { describeKeyShape, upstreamConfig } from "../src/upstream.js";

const here = path.dirname(fileURLToPath(import.meta.url));

/** Reads `.dev.vars`, which is TOML-ish: KEY = "value" or KEY=value. */
function readDevVars() {
  const file = path.join(here, "..", ".dev.vars");
  if (!fs.existsSync(file)) return {};
  const vars = {};
  for (const line of fs.readFileSync(file, "utf8").split("\n")) {
    const match = line.match(/^\s*([A-Z0-9_]+)\s*=\s*(.*?)\s*$/);
    if (!match) continue;
    vars[match[1]] = match[2].replace(/^["']|["']$/g, "");
  }
  return vars;
}

// The environment wins, so a one-off override on the command line works.
const env = { ...readDevVars(), ...process.env };
const config = upstreamConfig(env);

const say = (ok, text) => console.log(`${ok ? "  ok " : "FAIL "}${text}`);
const warn = (text) => console.log(`warn ${text}`);
let failed = false;
const fail = (text, hint) => {
  failed = true;
  say(false, text);
  if (hint) console.log(`      ${hint}`);
};

console.log("upstream");
say(true, `kind      ${config.kind}`);
say(true, `base url  ${config.baseURL || "https://api.anthropic.com (default)"}`);
say(true, `model     ${config.model}`);
say(true, `max tokens ${config.maxTokens}, effort ${config.effort}`);

if (!config.apiKey) {
  fail("no UPSTREAM_API_KEY", "Set it in .dev.vars or the environment.");
} else {
  say(true, `api key   ${describeKeyShape(config.apiKey)}, ${config.apiKey.length} chars`);
  if (config.apiKeyHadWhitespace) {
    warn("UPSTREAM_API_KEY had surrounding whitespace and was trimmed");
    console.log('      `echo "sk-…" | wrangler secret put` stores the newline too.');
  }
}

if (config.baseURL) {
  try {
    const parsed = new URL(config.baseURL);
    if (parsed.protocol !== "https:" && parsed.hostname !== "127.0.0.1" && parsed.hostname !== "localhost") {
      fail(
        `base url is ${parsed.protocol}//`,
        "A router reached over plaintext exposes the key on the path.",
      );
    }
  } catch {
    fail("base url is not a URL", "Include the scheme: https://router.example.com");
  }
}

console.log("\naccess control");
const keys = String(env.ASSISTANT_API_KEYS || "").split(",").map((k) => k.trim()).filter(Boolean);
const isPublic = /^(1|true|yes|on)$/i.test(String(env.ASSISTANT_PUBLIC || "").trim());
if (isPublic) {
  say(true, "ASSISTANT_PUBLIC is set — the endpoint answers without a key");
  console.log("      Anyone who finds the URL spends your model credit.");
} else if (keys.length === 0) {
  fail(
    "no ASSISTANT_API_KEYS and not public",
    "The deployment will refuse every request with 503 not_configured.",
  );
} else {
  say(true, `${keys.length} api key${keys.length === 1 ? "" : "s"} configured`);
  // The mix-up that reads as "the router rejected your key" when the router is
  // in fact describing a completely different key.
  if (keys.includes(config.apiKey)) {
    fail(
      "UPSTREAM_API_KEY is one of ASSISTANT_API_KEYS",
      "The two secrets are swapped. UPSTREAM_API_KEY is your router's key; " +
        "ASSISTANT_API_KEYS is what callers of this deployment present.",
    );
  } else if (
    config.apiKey &&
    !["other", "unset"].includes(describeKeyShape(config.apiKey)) &&
    keys.some((k) => describeKeyShape(k) === describeKeyShape(config.apiKey))
  ) {
    warn(
      `UPSTREAM_API_KEY has the same shape (${describeKeyShape(config.apiKey)}) as an assistant key`,
    );
    console.log("      If the router rejects its format, they are swapped.");
  }
  const weak = keys.filter((k) => k.length < 24);
  if (weak.length) {
    // A warning, not a failure: a short key still works, it is just guessable.
    warn(`${weak.length} key(s) shorter than 24 characters`);
    console.log("      head -c 32 /dev/urandom | base64");
  }
}

if (failed || !config.apiKey) {
  console.log("\nFix the above, then run this again.");
  process.exit(1);
}

console.log("\nlive check");
const { streamCompletion } = await import("../src/upstream.js");
let text = "";
let done = null;
const started = Date.now();
try {
  for await (const event of streamCompletion({
    config,
    system: "Reply with exactly: ok",
    messages: [{ role: "user", content: "Reply with exactly: ok" }],
    maxTokens: 64,
  })) {
    if (event.type === "text") text += event.text;
    if (event.type === "done") done = event;
  }
} catch (error) {
  fail(`the upstream refused the request: ${error.message}`);
  const hints = {
    upstream_auth_failed: "UPSTREAM_API_KEY is wrong for this router.",
    upstream_bad_request:
      "Usually UPSTREAM_MODEL is not a model id this router serves. Ask the router for its model list.",
    upstream_unreachable:
      "UPSTREAM_BASE_URL is wrong, or the router is down. The endpoint called is <base>/v1/chat/completions.",
    upstream_rate_limited: "The router is rate limiting. Retry shortly.",
  };
  if (hints[error.code]) console.log(`      ${hints[error.code]}`);
  process.exit(1);
}

const elapsed = Date.now() - started;
say(true, `answered in ${elapsed} ms`);
say(true, `served by  ${done?.model ?? "unknown"}`);
say(true, `stop       ${done?.stopReason ?? "unknown"}`);
if (done?.usage) {
  say(true, `tokens     ${done.usage.input} in, ${done.usage.output} out`);
}
if (!text.trim()) {
  fail("the answer was empty", "The router accepted the request but returned no content.");
  process.exit(1);
}
say(true, `answer     ${JSON.stringify(text.slice(0, 60))}`);
console.log("\nReady. `npx wrangler deploy` will work with these values.");
