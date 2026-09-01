// Vercel entry point.
//
// The Edge runtime is the same Web-API surface the handler is written against,
// and `process.env` carries the project's environment variables.

import { handle } from "../src/app.js";

export const config = { runtime: "edge" };

export default async function handler(request) {
  return handle(request, process.env);
}
