// Yandex Cloud Functions entry point.
//
// Yandex invokes a Node handler with an API-Gateway-shaped event and expects a
// plain object back, so this adapter builds a `Request`, runs the same handler
// as the other two targets, and flattens the `Response`.
//
// **Streaming does not stream here.** A cloud function returns one buffered
// response, so an SSE body is collected in full and delivered at the end. The
// wire format stays valid — an OpenAI client parses it and the mini-site
// renders it — but tokens arrive together rather than as they are produced.
// Raise the function's execution timeout accordingly; the default is far below
// what a long answer needs.
//
//   zip -r function.zip adapters src package.json node_modules
//   yc serverless function version create \
//     --function-name telemt-assistant --runtime nodejs18 \
//     --entrypoint adapters/yandex.handler --memory 256m --execution-timeout 300s \
//     --source-path function.zip --environment UPSTREAM_API_KEY=...

import { handle } from "../src/app.js";

/** Rebuilds a `Request` from the event Yandex passes in. */
function toRequest(event) {
  const headers = new Headers();
  for (const [name, value] of Object.entries(event.headers ?? {})) {
    if (value !== undefined && value !== null) headers.set(name, String(value));
  }
  for (const [name, values] of Object.entries(event.multiValueHeaders ?? {})) {
    if (Array.isArray(values) && values.length > 1) {
      headers.set(name, values.join(", "));
    }
  }

  const method = (event.httpMethod || "GET").toUpperCase();
  const path = event.url || event.path || "/";
  const query = new URLSearchParams();
  for (const [name, value] of Object.entries(event.queryStringParameters ?? {})) {
    if (value !== undefined && value !== null) query.append(name, String(value));
  }
  const search = query.toString();
  // The event carries no origin, and the handler only needs the path, so any
  // stable base works. `Host` keeps the site's own links pointing at itself.
  const host = headers.get("host") || "localhost";
  const scheme = headers.get("x-forwarded-proto") || "https";
  const url = `${scheme}://${host}${path}${search ? `?${search}` : ""}`;

  let body;
  if (method !== "GET" && method !== "HEAD" && event.body) {
    body = event.isBase64Encoded
      ? Buffer.from(event.body, "base64")
      : event.body;
  }

  return new Request(url, { method, headers, body });
}

export async function handler(event) {
  let response;
  try {
    response = await handle(toRequest(event), process.env);
  } catch (error) {
    return {
      statusCode: 500,
      headers: { "content-type": "application/json; charset=utf-8" },
      body: JSON.stringify({
        error: {
          message: error?.message || "Internal error",
          type: "internal_error",
        },
      }),
    };
  }

  const headers = {};
  for (const [name, value] of response.headers) headers[name] = value;
  // The client would otherwise wait for events that cannot arrive incrementally.
  if ((headers["content-type"] || "").startsWith("text/event-stream")) {
    headers["x-buffered-stream"] = "yandex-cloud-functions";
  }

  return {
    statusCode: response.status,
    headers,
    body: await response.text(),
    isBase64Encoded: false,
  };
}

// Yandex resolves `--entrypoint adapters/yandex.handler` against the module's
// exports; the default export keeps `adapters/yandex.default` working too.
export default handler;
