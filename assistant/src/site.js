import { SUGGESTIONS } from "./knowledge.js";
import { SERVED_MODEL } from "./openai.js";

/**
 * The mini-site.
 *
 * Three files served from this worker and nothing else: no bundler, no CDN, no
 * external font. That keeps the content security policy at `'self'` with no
 * inline exception, and it means the whole thing deploys as one artifact to any
 * of the three targets.
 */

const escapeHtml = (value) =>
  String(value)
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;");

export function indexHtml({ sealed, needsKey }) {
  const chips = SUGGESTIONS.map(
    (entry) =>
      `<button class="chip" type="button" data-prompt="${escapeHtml(entry.prompt)}">${escapeHtml(entry.title)}</button>`,
  ).join("");

  return `<!doctype html>
<html lang="en" class="dark">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1, viewport-fit=cover">
<meta name="color-scheme" content="dark light">
<meta name="robots" content="noindex, nofollow">
<title>telemt assistant</title>
<link rel="icon" href="/favicon.svg" type="image/svg+xml">
<link rel="stylesheet" href="/app.css">
</head>
<body data-sealed="${sealed ? "1" : "0"}" data-needs-key="${needsKey ? "1" : "0"}">
<header class="top">
  <div class="brand">
    <span class="mark">t</span>
    <span class="brand-text">
      <strong>telemt assistant</strong>
      <span class="sub">configs, explanations, debugging</span>
    </span>
  </div>
  <div class="top-actions">
    <button id="api-btn" class="ghost" type="button">API</button>
    <button id="key-btn" class="ghost" type="button" hidden>Key</button>
    <button id="clear-btn" class="ghost" type="button">New chat</button>
  </div>
</header>

<main id="main">
  <div id="thread" class="thread" aria-live="polite">
    <section class="welcome" id="welcome">
      <h1>Ask about telemt.</h1>
      <p>Configuration, the panel, federation, fake-TLS, the Middle-End pool, and
         the error it printed at three in the morning. Answers come back in the
         language you ask in.</p>
      <div class="chips">${chips}</div>
    </section>
  </div>
</main>

<form id="composer" class="composer" autocomplete="off">
  <textarea id="input" rows="1" placeholder="Describe what you want to run, or paste a config…" aria-label="Message"></textarea>
  <button id="send" type="submit" aria-label="Send">
    <svg viewBox="0 0 24 24" width="16" height="16" aria-hidden="true"><path d="M4 12h15M13 6l6 6-6 6" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="square"/></svg>
  </button>
</form>
<p class="disclaimer">Answers are generated and can be wrong. Check anything that
touches a live proxy against <code>docs/Config_params</code> before applying it.</p>

<dialog id="api-dialog">
  <form method="dialog">
    <h2>OpenAI-compatible endpoint</h2>
    <p>Point any OpenAI client at this origin. The model id is
       <code>${escapeHtml(SERVED_MODEL)}</code>.</p>
    <pre id="curl-sample"></pre>
    <div class="row">
      <button value="close" class="ghost">Close</button>
    </div>
  </form>
</dialog>

<dialog id="key-dialog">
  <form method="dialog" id="key-form">
    <h2>API key</h2>
    <p>This deployment requires a key. It is kept in this browser only and sent
       as a bearer token.</p>
    <input id="key-input" type="password" placeholder="sk-…" autocomplete="off" spellcheck="false">
    <div class="row">
      <button value="cancel" class="ghost" type="button" id="key-cancel">Cancel</button>
      <button value="save" id="key-save">Save</button>
    </div>
  </form>
</dialog>

<script src="/app.js" type="module"></script>
</body>
</html>`;
}

export const FAVICON = `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 32 32">
  <rect width="32" height="32" rx="7" fill="#000"/>
  <path d="M8 11h16M16 11v13" stroke="#fff" stroke-width="2.6" stroke-linecap="square"/>
</svg>`;

export const APP_CSS = `:root{
  --bg:#000;--fg:#f5f5f6;--muted:#8f9195;--card:#0a0a0b;--border:#1e1e21;
  --input:#2a2a2e;--accent:#17171a;--danger:#ff6b6b;
  --mono:ui-monospace,"SF Mono",SFMono-Regular,"JetBrains Mono",Menlo,monospace;
  --sans:ui-sans-serif,system-ui,-apple-system,"Segoe UI",Roboto,"Helvetica Neue",Arial,sans-serif;
}
@media (prefers-color-scheme: light){
  :root{--bg:#fff;--fg:#08090a;--muted:#5f6265;--card:#fff;--border:#e4e4e7;
        --input:#d8d8dc;--accent:#f4f4f5;--danger:#b4232a;}
}
*{box-sizing:border-box}
html,body{height:100%}
body{margin:0;display:flex;flex-direction:column;background:var(--bg);color:var(--fg);
  font-family:var(--sans);-webkit-font-smoothing:antialiased}
::selection{background:var(--fg);color:var(--bg)}
:focus-visible{outline:2px solid var(--fg);outline-offset:2px}

.top{display:flex;align-items:center;justify-content:space-between;gap:12px;
  padding:10px 16px;border-bottom:1px solid var(--border);position:sticky;top:0;
  background:color-mix(in oklab,var(--bg) 88%,transparent);backdrop-filter:blur(8px);z-index:5}
.brand{display:flex;align-items:center;gap:10px;min-width:0}
.mark{display:grid;place-items:center;width:26px;height:26px;border-radius:6px;
  background:var(--fg);color:var(--bg);font-weight:700;font-size:14px;line-height:1}
.brand-text{display:flex;flex-direction:column;min-width:0}
.brand-text strong{font-size:14px;letter-spacing:-.01em}
.sub{font-size:11px;color:var(--muted)}
.top-actions{display:flex;gap:6px;flex-shrink:0}

button{font:inherit;cursor:pointer;border-radius:6px;border:1px solid transparent;
  transition:background .15s,opacity .15s,border-color .15s}
.ghost{background:transparent;color:var(--muted);border-color:var(--input);
  padding:5px 10px;font-size:12px}
.ghost:hover{background:var(--accent);color:var(--fg)}

#main{flex:1;overflow-y:auto;overscroll-behavior:contain}
.thread{max-width:780px;margin:0 auto;padding:24px 16px 8px;
  display:flex;flex-direction:column;gap:20px}

.welcome h1{font-size:22px;letter-spacing:-.02em;margin:8px 0 6px}
.welcome p{color:var(--muted);font-size:14px;line-height:1.6;margin:0 0 18px;max-width:56ch}
.chips{display:flex;flex-wrap:wrap;gap:8px}
.chip{background:var(--card);border:1px solid var(--border);color:var(--fg);
  padding:7px 11px;font-size:12.5px;border-radius:999px}
.chip:hover{background:var(--accent)}

.msg{display:flex;flex-direction:column;gap:6px}
.who{font-size:10.5px;text-transform:uppercase;letter-spacing:.1em;color:var(--muted)}
.bubble{font-size:14.5px;line-height:1.65;word-wrap:break-word}
.msg.user .bubble{background:var(--card);border:1px solid var(--border);
  border-radius:10px;padding:10px 13px;white-space:pre-wrap}
.bubble > :first-child{margin-top:0}
.bubble > :last-child{margin-bottom:0}
.bubble p{margin:0 0 10px}
.bubble ul,.bubble ol{margin:0 0 10px;padding-left:20px}
.bubble li{margin:3px 0}
.bubble h2,.bubble h3{font-size:14.5px;letter-spacing:-.01em;margin:16px 0 8px}
.bubble code{font-family:var(--mono);font-size:12.5px;background:var(--accent);
  padding:1px 5px;border-radius:4px}
.bubble a{color:inherit}

.code{position:relative;margin:0 0 12px;border:1px solid var(--border);border-radius:8px;
  background:var(--card);overflow:hidden}
.code-head{display:flex;align-items:center;justify-content:space-between;
  padding:5px 8px 5px 11px;border-bottom:1px solid var(--border);
  font-size:10.5px;text-transform:uppercase;letter-spacing:.08em;color:var(--muted)}
.code pre{margin:0;padding:11px 13px;overflow-x:auto;font-family:var(--mono);
  font-size:12.5px;line-height:1.6}
.copy{background:transparent;border:1px solid var(--input);color:var(--muted);
  padding:2px 8px;font-size:10.5px;border-radius:5px;text-transform:none;letter-spacing:0}
.copy:hover{background:var(--accent);color:var(--fg)}

.thinking{border-left:2px solid var(--border);padding-left:11px;color:var(--muted);
  font-size:13px;line-height:1.6;white-space:pre-wrap;margin-bottom:10px}
.error{color:var(--danger);font-size:13px;border:1px solid color-mix(in oklab,var(--danger) 35%,transparent);
  border-radius:8px;padding:9px 12px;background:color-mix(in oklab,var(--danger) 8%,transparent)}
.cursor::after{content:"";display:inline-block;width:7px;height:14px;
  background:var(--fg);margin-left:2px;vertical-align:-2px;animation:blink 1s steps(2) infinite}
@keyframes blink{50%{opacity:0}}

.composer{max-width:780px;width:100%;margin:0 auto;padding:8px 16px 0;
  display:flex;gap:8px;align-items:flex-end}
#input{flex:1;resize:none;max-height:200px;background:var(--card);color:var(--fg);
  border:1px solid var(--input);border-radius:10px;padding:11px 13px;
  font:inherit;font-size:14.5px;line-height:1.5}
#input::placeholder{color:var(--muted)}
#input:focus{outline:none;border-color:var(--fg)}
#send{background:var(--fg);color:var(--bg);width:38px;height:38px;
  display:grid;place-items:center;flex-shrink:0}
#send:disabled{opacity:.35;cursor:default}
.disclaimer{max-width:780px;margin:8px auto 14px;padding:0 16px;
  font-size:11px;color:var(--muted);line-height:1.5}
.disclaimer code{font-family:var(--mono)}

dialog{border:1px solid var(--border);border-radius:10px;background:var(--card);
  color:var(--fg);max-width:min(560px,calc(100vw - 32px));padding:0}
dialog::backdrop{background:rgba(0,0,0,.7)}
dialog form{padding:18px}
dialog h2{margin:0 0 6px;font-size:15px;letter-spacing:-.01em}
dialog p{margin:0 0 12px;font-size:13px;color:var(--muted);line-height:1.6}
dialog pre{margin:0 0 14px;padding:11px;background:var(--accent);border-radius:8px;
  font-family:var(--mono);font-size:11.5px;overflow-x:auto;white-space:pre-wrap;word-break:break-all}
dialog input{width:100%;background:transparent;color:var(--fg);border:1px solid var(--input);
  border-radius:8px;padding:9px 11px;font:inherit;font-size:14px;margin-bottom:14px}
dialog input:focus{outline:none;border-color:var(--fg)}
.row{display:flex;gap:8px;justify-content:flex-end}
.row button:not(.ghost){background:var(--fg);color:var(--bg);padding:6px 14px;font-size:13px}

@media (max-width:560px){
  .sub{display:none}
  .thread{padding-top:16px}
}`;

export const APP_JS = String.raw`const KEY_STORAGE = "telemt.assistant.key";
const thread = document.getElementById("thread");
const welcome = document.getElementById("welcome");
const form = document.getElementById("composer");
const input = document.getElementById("input");
const send = document.getElementById("send");
const keyButton = document.getElementById("key-btn");
const keyDialog = document.getElementById("key-dialog");
const keyInput = document.getElementById("key-input");
const apiDialog = document.getElementById("api-dialog");

const needsKey = document.body.dataset.needsKey === "1";
const sealed = document.body.dataset.sealed === "1";
let history = [];
let inFlight = null;

if (needsKey) keyButton.hidden = false;

document.getElementById("curl-sample").textContent =
  "curl " + location.origin + "/v1/chat/completions \\\n" +
  "  -H 'content-type: application/json' \\\n" +
  (needsKey ? "  -H 'authorization: Bearer YOUR_KEY' \\\n" : "") +
  "  -d '{\"model\":\"telemt-assistant\",\"stream\":true,\n" +
  "       \"messages\":[{\"role\":\"user\",\"content\":\"Minimal config with the panel?\"}]}'";

document.getElementById("api-btn").addEventListener("click", () => apiDialog.showModal());
keyButton.addEventListener("click", () => {
  keyInput.value = localStorage.getItem(KEY_STORAGE) || "";
  keyDialog.showModal();
});
document.getElementById("key-cancel").addEventListener("click", () => keyDialog.close());
document.getElementById("key-form").addEventListener("submit", (event) => {
  if (event.submitter && event.submitter.value === "save") {
    const value = keyInput.value.trim();
    if (value) localStorage.setItem(KEY_STORAGE, value);
    else localStorage.removeItem(KEY_STORAGE);
  }
});
document.getElementById("clear-btn").addEventListener("click", () => {
  if (inFlight) inFlight.abort();
  history = [];
  thread.replaceChildren(welcome);
  welcome.hidden = false;
});

for (const chip of document.querySelectorAll(".chip")) {
  chip.addEventListener("click", () => {
    input.value = chip.dataset.prompt;
    autosize();
    input.focus();
  });
}

function autosize() {
  input.style.height = "auto";
  input.style.height = Math.min(input.scrollHeight, 200) + "px";
}
input.addEventListener("input", autosize);
input.addEventListener("keydown", (event) => {
  if (event.key === "Enter" && !event.shiftKey) {
    event.preventDefault();
    form.requestSubmit();
  }
});

if (sealed) {
  showError(
    "This deployment has no API keys configured, so it will not answer. " +
    "Set ASSISTANT_API_KEYS, or ASSISTANT_PUBLIC=1 to serve without auth.",
  );
}

form.addEventListener("submit", async (event) => {
  event.preventDefault();
  const text = input.value.trim();
  if (!text || inFlight) return;
  if (needsKey && !localStorage.getItem(KEY_STORAGE)) {
    keyDialog.showModal();
    return;
  }

  welcome.hidden = true;
  input.value = "";
  autosize();
  appendMessage("you", text, true);
  history.push({ role: "user", content: text });

  const bubble = appendMessage("assistant", "", false);
  bubble.classList.add("cursor");
  send.disabled = true;
  inFlight = new AbortController();

  let answer = "";
  let reasoning = "";
  let reasoningNode = null;

  try {
    const headers = { "content-type": "application/json" };
    const key = localStorage.getItem(KEY_STORAGE);
    if (key) headers.authorization = "Bearer " + key;

    const response = await fetch("/v1/chat/completions", {
      method: "POST",
      headers,
      signal: inFlight.signal,
      body: JSON.stringify({
        model: "telemt-assistant",
        stream: true,
        messages: history,
      }),
    });

    if (!response.ok || !response.body) {
      const payload = await response.json().catch(() => null);
      throw new Error(
        (payload && payload.error && payload.error.message) ||
          "Request failed with status " + response.status,
      );
    }

    for await (const payload of sse(response.body, inFlight.signal)) {
      if (payload === "[DONE]") break;
      let chunk;
      try {
        chunk = JSON.parse(payload);
      } catch {
        continue;
      }
      const delta = (chunk.choices && chunk.choices[0] && chunk.choices[0].delta) || {};
      if (typeof delta.reasoning_content === "string" && delta.reasoning_content) {
        reasoning += delta.reasoning_content;
        if (!reasoningNode) {
          reasoningNode = document.createElement("div");
          reasoningNode.className = "thinking";
          bubble.parentNode.insertBefore(reasoningNode, bubble);
        }
        reasoningNode.textContent = reasoning;
        scrollToEnd();
      }
      if (typeof delta.content === "string" && delta.content) {
        answer += delta.content;
        render(bubble, answer);
        scrollToEnd();
      }
    }
    if (answer) history.push({ role: "assistant", content: answer });
    else showError("The assistant returned an empty response.");
  } catch (error) {
    if (error.name !== "AbortError") showError(error.message);
  } finally {
    bubble.classList.remove("cursor");
    send.disabled = false;
    inFlight = null;
    input.focus();
  }
});

async function* sse(body, signal) {
  const reader = body.getReader();
  const decoder = new TextDecoder();
  let buffer = "";
  try {
    while (!signal.aborted) {
      const { done, value } = await reader.read();
      if (done) break;
      buffer += decoder.decode(value, { stream: true });
      let boundary;
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

function appendMessage(who, text, isUser) {
  const wrapper = document.createElement("div");
  wrapper.className = "msg " + (isUser ? "user" : "assistant");
  const label = document.createElement("div");
  label.className = "who";
  label.textContent = who;
  const bubble = document.createElement("div");
  bubble.className = "bubble";
  if (isUser) bubble.textContent = text;
  wrapper.append(label, bubble);
  thread.append(wrapper);
  scrollToEnd();
  return bubble;
}

function showError(message) {
  const node = document.createElement("div");
  node.className = "error";
  node.textContent = message;
  thread.append(node);
  scrollToEnd();
}

function scrollToEnd() {
  const main = document.getElementById("main");
  const nearBottom = main.scrollHeight - main.scrollTop - main.clientHeight < 140;
  if (nearBottom) main.scrollTop = main.scrollHeight;
}

/**
 * Renders the small Markdown subset the model actually emits.
 *
 * Everything is built with textContent and createElement, never innerHTML: the
 * text comes from a model that can be steered by whatever an operator pasted
 * into the chat, and that is not a source any page should hand to a parser.
 */
function render(target, markdown) {
  target.replaceChildren();
  const lines = markdown.split("\n");
  let index = 0;
  let paragraph = [];
  let list = null;

  const flushParagraph = () => {
    if (!paragraph.length) return;
    const node = document.createElement("p");
    inline(node, paragraph.join(" "));
    target.append(node);
    paragraph = [];
  };
  const flushList = () => {
    if (list) target.append(list);
    list = null;
  };

  while (index < lines.length) {
    const line = lines[index];
    const fence = line.match(/^\s*\`\`\`(\w*)\s*$/);
    if (fence) {
      flushParagraph();
      flushList();
      const language = fence[1] || "text";
      const collected = [];
      index += 1;
      while (index < lines.length && !/^\s*\`\`\`\s*$/.test(lines[index])) {
        collected.push(lines[index]);
        index += 1;
      }
      index += 1;
      target.append(codeBlock(language, collected.join("\n")));
      continue;
    }

    const heading = line.match(/^(#{1,6})\s+(.*)$/);
    if (heading) {
      flushParagraph();
      flushList();
      const node = document.createElement(heading[1].length <= 2 ? "h2" : "h3");
      inline(node, heading[2]);
      target.append(node);
      index += 1;
      continue;
    }

    const bullet = line.match(/^\s*[-*]\s+(.*)$/);
    const numbered = line.match(/^\s*\d+[.)]\s+(.*)$/);
    if (bullet || numbered) {
      flushParagraph();
      const wanted = bullet ? "UL" : "OL";
      if (!list || list.tagName !== wanted) {
        flushList();
        list = document.createElement(bullet ? "ul" : "ol");
      }
      const item = document.createElement("li");
      inline(item, (bullet || numbered)[1]);
      list.append(item);
      index += 1;
      continue;
    }

    if (line.trim() === "") {
      flushParagraph();
      flushList();
    } else {
      flushList();
      paragraph.push(line.trim());
    }
    index += 1;
  }
  flushParagraph();
  flushList();
}

function codeBlock(language, text) {
  const wrapper = document.createElement("div");
  wrapper.className = "code";
  const head = document.createElement("div");
  head.className = "code-head";
  const name = document.createElement("span");
  name.textContent = language;
  const copy = document.createElement("button");
  copy.type = "button";
  copy.className = "copy";
  copy.textContent = "Copy";
  copy.addEventListener("click", async () => {
    try {
      await navigator.clipboard.writeText(text);
      copy.textContent = "Copied";
      setTimeout(() => (copy.textContent = "Copy"), 1500);
    } catch {
      copy.textContent = "Press Ctrl+C";
    }
  });
  head.append(name, copy);
  const pre = document.createElement("pre");
  pre.textContent = text;
  wrapper.append(head, pre);
  return wrapper;
}

/** Inline code and bold only — the two the model uses that matter. */
function inline(target, text) {
  const pattern = /(\`[^\`]+\`|\*\*[^*]+\*\*)/g;
  let cursor = 0;
  let match;
  while ((match = pattern.exec(text)) !== null) {
    if (match.index > cursor) {
      target.append(document.createTextNode(text.slice(cursor, match.index)));
    }
    const token = match[0];
    if (token.startsWith("\`")) {
      const code = document.createElement("code");
      code.textContent = token.slice(1, -1);
      target.append(code);
    } else {
      const strong = document.createElement("strong");
      strong.textContent = token.slice(2, -2);
      target.append(strong);
    }
    cursor = match.index + token.length;
  }
  if (cursor < text.length) {
    target.append(document.createTextNode(text.slice(cursor)));
  }
}

autosize();
input.focus();
`;
