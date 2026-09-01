// Cloudflare Workers entry point.
//
// Secrets arrive as bindings on `env`, which is already the shape the handler
// expects, so there is nothing to translate.
//
//   wrangler secret put UPSTREAM_API_KEY
//   wrangler secret put ASSISTANT_API_KEYS
//   wrangler deploy

import { handle } from "../src/app.js";

export default {
  async fetch(request, env) {
    return handle(request, env);
  },
};
