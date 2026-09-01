# telemt assistant

A small site and an OpenAI-compatible endpoint with Claude behind them, briefed
on telemt. It writes and reviews `config.toml`, explains what a setting actually
does, and works through a deployment that will not start.

Separate from the proxy on purpose: nothing here ships in the `telemt` binary,
and the assistant has no access to any running node. It reads what you paste and
answers.

- **The site** — a chat page at `/`, no build step, no external request. One
  HTML document, one stylesheet, one module, all served by the same handler.
- **The API** — `POST /v1/chat/completions` and `GET /v1/models` in the OpenAI
  shape, streaming or not, so any client you already have works unchanged.

Deploys to **Cloudflare Workers**, **Vercel**, or **Yandex Cloud Functions**
from the same source: the handler is `(Request, env) => Response` written
against Web APIs only, and each platform gets a three-line adapter.

## Quick start

```bash
npm install
npm run check          # syntax + 26 end-to-end tests, no credential needed
```

Then pick a target.

### Cloudflare Workers

```bash
npx wrangler secret put UPSTREAM_API_KEY
npx wrangler secret put ASSISTANT_API_KEYS
npx wrangler deploy
```

For local development put the same values in `.dev.vars` (gitignored) and run
`npx wrangler dev`.

### Vercel

```bash
vercel env add UPSTREAM_API_KEY
vercel env add ASSISTANT_API_KEYS
vercel deploy --prod
```

`vercel.json` rewrites every path to the edge function, so the site and the API
share one origin.

### Yandex Cloud Functions

```bash
npm run package:yandex

yc serverless function create --name telemt-assistant
yc serverless function version create \
  --function-name telemt-assistant \
  --runtime nodejs18 --entrypoint adapters/yandex.handler \
  --memory 256m --execution-timeout 300s \
  --source-path yandex-function.zip \
  --environment UPSTREAM_API_KEY=...,ASSISTANT_API_KEYS=...
```

Put it behind an API Gateway with a `{proxy+}` route so the site's own paths
resolve.

> **Streaming does not stream on Yandex.** A cloud function returns one buffered
> response, so an SSE body is collected in full and delivered at the end. The
> format stays valid — clients parse it, the site renders it — but tokens arrive
> together. The response carries `X-Buffered-Stream: yandex-cloud-functions` so
> this is visible rather than mysterious. Raise the execution timeout; the
> default is far below what a long answer needs.

## Pointing it at your router

The upstream is pluggable, because most people running this already have a
router in front of their model rather than a direct account.

**A router that speaks the Anthropic Messages API** — the common shape for a
"Claude router" — needs one variable:

```
UPSTREAM_BASE_URL = https://router.example.com
UPSTREAM_API_KEY  = <the router's key>
```

**A router that only speaks OpenAI Chat Completions** (OpenRouter and friends):

```
UPSTREAM_KIND     = openai
UPSTREAM_BASE_URL = https://openrouter.ai/api
UPSTREAM_API_KEY  = <the router's key>
UPSTREAM_MODEL    = anthropic/claude-opus-5
```

Prefer the Anthropic shape where you have the choice. Effort and adaptive
thinking live in the Messages API, and a router that flattens it to Chat
Completions drops both.

Leaving `UPSTREAM_BASE_URL` unset uses `api.anthropic.com` directly.

## Configuration

| Variable | Default | Meaning |
| --- | --- | --- |
| `UPSTREAM_API_KEY` | — | Credential for the model. Required. `ANTHROPIC_API_KEY` is read as a fallback. |
| `UPSTREAM_BASE_URL` | Anthropic's own | Where the upstream lives. `ANTHROPIC_BASE_URL` is read as a fallback. |
| `UPSTREAM_KIND` | `anthropic` | `anthropic` (Messages API) or `openai` (Chat Completions). |
| `UPSTREAM_MODEL` | `claude-opus-5`, or `anthropic/claude-opus-5` for `openai` | Model id to ask the upstream for. |
| `UPSTREAM_EFFORT` | `high` | `low`–`max`. Messages API only. |
| `UPSTREAM_MAX_TOKENS` | `16000` | Output ceiling when the caller names none. |
| `UPSTREAM_SHOW_THINKING` | `false` | Stream a summary of the reasoning as `reasoning_content`. |
| `UPSTREAM_FALLBACKS` | on for `api.anthropic.com` | Server-side refusal fallbacks. A router will reject the beta parameter, so it is off by default anywhere else. |
| `ASSISTANT_API_KEYS` | — | Comma-separated keys accepted as `Authorization: Bearer`. |
| `ASSISTANT_PUBLIC` | `false` | Serve with no authentication at all. |
| `ASSISTANT_RATE_LIMIT` | off | Requests per window, per key or address. |
| `ASSISTANT_RATE_WINDOW_SECS` | `60` | The window. |
| `ASSISTANT_CORS_ORIGINS` | — | Origins allowed to call the API from a browser. `*` allows any. |

## Security

**It fails closed.** With no `ASSISTANT_API_KEYS` and no `ASSISTANT_PUBLIC=1`,
every API request is refused with `503 not_configured` and the site says why.
An OpenAI-compatible endpoint left open is a faucet for your model credit, and
it will be found — so opening it has to be a decision, not an oversight.

**Keys are compared in constant time**, and every configured key is compared on
every request, so the answer does not leak which one matched.

**The rate limit is best-effort.** Serverless runtimes give no shared state, so
the built-in throttle bounds one warm isolate and no more. It stops one client
hammering one instance; it is not a quota. For a real one use Cloudflare Rate
Limiting rules, a Vercel firewall rule, or an API Gateway quota in Yandex Cloud.

**The page has no inline script or style**, so its content security policy is
`'self'` with no exception. Model output is rendered by building DOM nodes, not
by handing a string to an HTML parser — the text comes from a model that can be
steered by whatever someone pasted into the chat.

**The site never embeds a key.** When the deployment requires one, the page
asks for it and keeps it in that browser's local storage.

## How it knows about telemt

`src/knowledge.js` is the prose: what telemt is, how the sections fit together,
what this fork adds, and the failure modes worth recognising.

`src/schema.js` is generated from telemt's own strict-key tables — every one of
the 565 configuration keys the proxy validates against, in 35 sections:

```bash
node tools/generate-schema.mjs          # regenerate
node tools/generate-schema.mjs --check  # fail if stale
```

Generating rather than typing that list is the point. The assistant's worst
failure mode is inventing a plausible key, and a key added to telemt cannot
silently go missing here. Run `--check` after any change to
`src/config/load/strict_keys.rs` in the proxy.

## The API

```bash
curl https://your-deployment/v1/chat/completions \
  -H 'content-type: application/json' \
  -H 'authorization: Bearer YOUR_KEY' \
  -d '{
    "model": "telemt-assistant",
    "stream": true,
    "messages": [{"role": "user", "content": "Master and two agents. Configs?"}]
  }'
```

| Route | Purpose |
| --- | --- |
| `GET /` | The chat page. |
| `POST /v1/chat/completions` | Streaming or blocking completion. |
| `GET /v1/models` | Advertises one model, `telemt-assistant`. |
| `GET /healthz` | Liveness, no credential. |

The served model id is always `telemt-assistant` regardless of what the upstream
runs — clients pin a model name, and the operator should be free to change what
is behind it.

A `system` turn from the caller is **appended** to the assistant's own prompt
rather than replacing it. The telemt knowledge is the product; a client that
sends its own persona refines it.

## What it does not do

- **It cannot reach your proxy.** No node introspection, no live validation. It
  reads what you paste.
- **It does not validate TOML against the binary.** For that, run
  `telemt run config.toml` on the host and read the error, which names the key.
- **It has no memory between page loads.** Conversation state lives in the tab.
- **It is not a substitute for the reference.** Check anything that touches a
  live proxy against `docs/Config_params/`.
