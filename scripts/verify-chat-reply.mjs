// Verify the tolerant reply reader against the real provider that failed.
//
// Loads the same config the app uses, sends the same non-streaming request that
// chat.rs sends, and applies the same two-step read (leading JSON value, else
// fold the `data:` frames). Prints the text the chat panel would now show.
//
//   node scripts/verify-chat-reply.mjs 9router
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { homedir } from "node:os";

const name = process.argv[2] || "9router";
const cfgPath = join(process.env.LOCALAPPDATA || join(homedir(), "AppData", "Local"), "rustbox", "config.json");
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

const body = {
  model,
  messages: [
    { role: "system", content: "You are a helpful assistant." },
    { role: "user", content: "Reply with the single word: ok" },
  ],
};

const headers = { "content-type": "application/json" };
for (const h of p.extra_headers || p.headers || []) if (h?.name) headers[h.name] = h.value;
if (key && !headers.authorization) headers.authorization = `Bearer ${key}`;

const res = await fetch(url, { method: "POST", headers, body: JSON.stringify(body) });
const text = await res.text();
console.log(`status=${res.status} content-type=${res.headers.get("content-type")}`);

// --- the same logic as chat.rs::read_reply_body -------------------------
const trimmed = text.replace(/^\uFEFF/, "").trim();
let value = null;
try {
  value = JSON.parse(trimmed);
  console.log("read=plain-json");
} catch {
  // Leading JSON value followed by anything else (e.g. `data: [DONE]`).
  const first = firstJsonValue(trimmed);
  if (first !== null) {
    value = first;
    console.log("read=leading-json-value");
  } else {
    value = foldEventStream(trimmed);
    console.log(value ? "read=folded-stream" : "read=none");
  }
}

function firstJsonValue(s) {
  // Mirror serde_json's stream deserializer: decode one value, ignore the rest.
  for (let end = s.length; end > 0; end--) {
    const slice = s.slice(0, end).trimEnd();
    if (!slice.endsWith("}")) continue;
    try {
      return JSON.parse(slice);
    } catch {
      /* keep shrinking */
    }
  }
  return null;
}

function foldEventStream(s) {
  let out = "";
  const calls = [];
  let saw = false;
  for (const raw of s.split(/\r?\n/)) {
    const line = raw.trim();
    if (!line.startsWith("data:")) continue;
    const payload = line.slice(5).trim();
    if (!payload || payload === "[DONE]") continue;
    let frame;
    try {
      frame = JSON.parse(payload);
    } catch {
      continue;
    }
    saw = true;
    const choice = frame.choices?.[0];
    if (choice?.message) return frame;
    if (choice?.delta) {
      if (typeof choice.delta.content === "string") out += choice.delta.content;
      for (const tc of choice.delta.tool_calls || []) {
        const idx = tc.index ?? calls.length;
        let slot = calls.find((c) => c.index === idx);
        if (!slot) calls.push((slot = { index: idx, id: "", name: "", args: "" }));
        if (tc.id) slot.id = tc.id;
        if (tc.function?.name) slot.name = tc.function.name;
        if (tc.function?.arguments) slot.args += tc.function.arguments;
      }
    }
    if (frame.type === "content_block_delta" && frame.delta?.text) out += frame.delta.text;
  }
  if (!saw) return null;
  return {
    choices: [
      {
        index: 0,
        finish_reason: "stop",
        message: {
          role: "assistant",
          content: out,
          tool_calls: calls.map((c) => ({ id: c.id, function: { name: c.name, arguments: c.args } })),
        },
      },
    ],
  };
}

const msg = value?.choices?.[0]?.message;
if (!msg) {
  console.log("RESULT: no message parsed");
  process.exit(1);
}
console.log(`RESULT text=${JSON.stringify(msg.content)}`);
console.log(`RESULT tool_calls=${msg.tool_calls?.length ?? 0}`);
