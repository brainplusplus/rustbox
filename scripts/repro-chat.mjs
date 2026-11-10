// Reproduce the chat panel's request path against a provider and show the raw
// reply. Diagnostics only: it prints no secrets and stores nothing.
//
//   node scripts/repro-chat.mjs 9router
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { homedir } from "node:os";

const name = process.argv[2] || "9router";
const cfgPath = join(process.env.LOCALAPPDATA || join(homedir(), "AppData", "Local"), "rustbox", "config.json");
// The app's config is written with a UTF-8 BOM, which JSON.parse rejects.
const cfg = JSON.parse(readFileSync(cfgPath, "utf8").replace(/^\uFEFF/, ""));
const p = (cfg.settings?.providers || []).find((x) => x.name === name);
if (!p) {
  console.error(`no provider named ${name}`);
  process.exit(1);
}

const base = String(p.base_url || "").replace(/\/+$/, "");
const url = base.endsWith("/v1") ? `${base}/chat/completions` : `${base}/v1/chat/completions`;
const key = p.api_key || p.keys?.[0]?.key || p.keys?.[0]?.value || "";
const model = p.model || (Array.isArray(p.models) ? p.models[0]?.id : p.models) || "";

// Mirrors chat.rs request_body for ChatCompletions: no `stream` field at all.
const body = {
  model,
  messages: [
    { role: "system", content: "You are a helpful assistant." },
    { role: "user", content: "Reply with the single word: ok" },
  ],
};

const headers = { "content-type": "application/json" };
for (const h of p.extra_headers || p.headers || []) {
  if (h?.name) headers[h.name] = h.value;
}
if (key && !headers.authorization) headers.authorization = `Bearer ${key}`;

console.log(`url=${url}`);
console.log(`model=${model}`);
console.log(`key=${key ? key.slice(0, 6) + "..." : "(none)"}`);
console.log(`headers=${Object.keys(headers).join(",")}`);

const res = await fetch(url, { method: "POST", headers, body: JSON.stringify(body) });
const text = await res.text();
console.log(`status=${res.status}`);
console.log(`content-type=${res.headers.get("content-type")}`);
console.log(`transfer-encoding=${res.headers.get("transfer-encoding")}`);
console.log(`bytes=${Buffer.byteLength(text)}`);

const first = text.indexOf("\n");
console.log(`first_newline=${first}`);
try {
  JSON.parse(text);
  console.log("parse=ok");
} catch (e) {
  console.log(`parse=FAIL ${e.message}`);
  const m = /column (\d+)/.exec(e.message);
  if (m) {
    const col = Number(m[1]);
    const from = Math.max(0, col - 120);
    console.log(`--- around column ${col} ---`);
    console.log(JSON.stringify(text.slice(from, col + 160)));
  }
}
console.log("--- first 300 chars ---");
console.log(JSON.stringify(text.slice(0, 300)));
