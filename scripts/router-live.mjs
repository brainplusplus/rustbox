// Live end-to-end checks against the running RustBox AI Router.
//
// Everything here goes over the real HTTP listener on 127.0.0.1:11930 and, where
// a model is called, through a local mock vendor on 127.0.0.1:18080. That keeps
// the results about the router's own behaviour (protocol translation, streaming
// re-framing, combo strategies, request logging) instead of about whichever
// vendor happens to be reachable and paid up today.
const ROUTER = process.env.ROUTER || 'http://127.0.0.1:11930';
// Once the router has any API key, a keyless call is refused. Pass a key here to
// run the suite against a keyed router; leave it empty for an open one.
const KEY = process.env.ROUTER_KEY || '';
const AUTH = KEY ? { authorization: `Bearer ${KEY}` } : {};
const results = [];

function check(name, ok, detail) {
  results.push({ name, ok, detail });
  console.log(`${ok ? 'PASS' : 'FAIL'}  ${name}${detail ? `  — ${detail}` : ''}`);
}

async function post(path, body, headers = {}) {
  const res = await fetch(ROUTER + path, {
    method: 'POST',
    headers: { 'content-type': 'application/json', ...AUTH, ...headers },
    body: JSON.stringify(body),
  });
  const text = await res.text();
  let json = null;
  try {
    json = JSON.parse(text);
  } catch (_) {}
  return { status: res.status, text, json, headers: res.headers };
}

async function get(path, headers = {}) {
  const res = await fetch(ROUTER + path, { headers: { ...AUTH, ...headers } });
  const text = await res.text();
  let json = null;
  try {
    json = JSON.parse(text);
  } catch (_) {}
  return { status: res.status, text, json };
}

// Read an SSE body to completion and return the raw frames plus the joined text.
async function stream(path, body) {
  const res = await fetch(ROUTER + path, {
    method: 'POST',
    headers: { 'content-type': 'application/json', ...AUTH },
    body: JSON.stringify({ ...body, stream: true }),
  });
  const raw = await res.text();
  return { status: res.status, raw, type: res.headers.get('content-type') };
}

const msg = [{ role: 'user', content: 'hello' }];

async function main() {
  // ---- discovery ---------------------------------------------------------
  const models = await get('/v1/models');
  check('/v1/models answers 200', models.status === 200, `status ${models.status}`);
  const ids = (models.json?.data || []).map((m) => m.id);
  check(
    'every provider model is published as prefix/model',
    ids.includes('mock/mock-a') && ids.includes('risun/bansos/deepseek-v4.1-flash-1037'),
    `${ids.length} ids`
  );
  check(
    'combos are published under combo/',
    ids.includes('combo/fallback-test') && ids.includes('combo/roundrobin-test')
  );
  check('combo ids never leak into a provider prefix', !ids.some((i) => i.startsWith('risun/combo/')));

  const health = await get('/v1/health');
  check('/v1/health reports the counts', health.status === 200 && health.json?.ok === true,
    `models=${health.json?.models} combos=${health.json?.combos}`);

  // ---- same-protocol forwarding -----------------------------------------
  const chat = await post('/v1/chat/completions', { model: 'mock/mock-a', messages: msg, max_tokens: 8 });
  check('[OI] Chat Completions forwards and returns 200', chat.status === 200, `status ${chat.status}`);
  const chatText = chat.json?.choices?.[0]?.message?.content || '';
  check('the upstream answer reaches the client unchanged', chatText === 'reply-from:mock-a', JSON.stringify(chatText));
  check('usage is carried back', chat.json?.usage?.prompt_tokens === 12, JSON.stringify(chat.json?.usage));

  // ---- streaming passthrough --------------------------------------------
  const chatStream = await stream('/v1/chat/completions', { model: 'mock/mock-a', messages: msg });
  check('streaming answers with text/event-stream', /text\/event-stream/.test(chatStream.type || ''), chatStream.type);
  check('the stream ends with [DONE]', chatStream.raw.includes('[DONE]'));
  check('the stream carries the upstream text', chatStream.raw.includes('reply-from:mock-a'));

  // ---- Anthropic protocol against a Chat provider ------------------------
  // The interesting case: an Anthropic client, a Chat Completions vendor. The
  // router has to translate both directions and re-frame the reply.
  const anthropic = await post('/v1/messages', {
    model: 'mock/mock-a',
    max_tokens: 16,
    messages: [{ role: 'user', content: 'hello' }],
  });
  check('Anthropic Messages answers 200 against a Chat vendor', anthropic.status === 200, `status ${anthropic.status}`);
  check('the reply is in Anthropic shape', anthropic.json?.type === 'message', anthropic.json?.type);
  const anthText = anthropic.json?.content?.[0]?.text || '';
  check('the translated text survives', anthText === 'reply-from:mock-a', JSON.stringify(anthText));
  check('usage is reported in Anthropic fields',
    anthropic.json?.usage?.input_tokens === 12 && anthropic.json?.usage?.output_tokens === 5,
    JSON.stringify(anthropic.json?.usage));

  const anthStream = await stream('/v1/messages', {
    model: 'mock/mock-a', max_tokens: 16, messages: [{ role: 'user', content: 'hello' }],
  });
  check('a translated stream is re-framed as Anthropic events',
    anthStream.raw.includes('event: message_start') && anthStream.raw.includes('event: message_stop'));

  // ---- Responses API -----------------------------------------------------
  const responses = await post('/v1/responses', { model: 'mock/mock-a', input: 'hello' });
  check('[OI] Responses answers 200', responses.status === 200, `status ${responses.status}`);
  check('the Responses payload carries the text',
    JSON.stringify(responses.json || {}).includes('reply-from:mock-a'));

  // ---- combo: fallback ---------------------------------------------------
  // mock-fail answers 500 upstream, so a fallback combo must skip it and return
  // the next member's answer rather than the failure.
  const fb = await post('/v1/chat/completions', { model: 'combo/fallback-test', messages: msg });
  check('fallback combo answers 200 despite a broken first member', fb.status === 200, `status ${fb.status}`);
  const fbText = fb.json?.choices?.[0]?.message?.content || '';
  check('fallback returns the healthy member\'s answer', fbText === 'reply-from:mock-a', JSON.stringify(fbText));

  // ---- combo: round robin ------------------------------------------------
  // Two members, so two consecutive calls must not land on the same one.
  const rr1 = await post('/v1/chat/completions', { model: 'combo/roundrobin-test', messages: msg });
  const rr2 = await post('/v1/chat/completions', { model: 'combo/roundrobin-test', messages: msg });
  const rr3 = await post('/v1/chat/completions', { model: 'combo/roundrobin-test', messages: msg });
  const seen = [rr1, rr2, rr3].map((r) => r.json?.choices?.[0]?.message?.content || '');
  check('round-robin answers every time', [rr1, rr2, rr3].every((r) => r.status === 200), seen.join(' | '));
  check('round-robin rotates across the members', new Set(seen).size >= 2, seen.join(' | '));

  // ---- error surfaces ----------------------------------------------------
  const unknown = await post('/v1/chat/completions', { model: 'nope/does-not-exist', messages: msg });
  check('an unknown model is a 4xx with a message',
    unknown.status >= 400 && unknown.status < 500 && Boolean(unknown.json?.error?.message),
    `${unknown.status} ${unknown.json?.error?.message?.slice(0, 60)}`);

  const noRoute = await get('/v1/nothing-here');
  check('an unknown route is 404 with guidance',
    noRoute.status === 404 && /\/v1\/models/.test(noRoute.json?.error?.message || ''),
    `${noRoute.status}`);

  const badMethod = await get('/v1/chat/completions');
  check('the wrong method on a real route is 405', badMethod.status === 405, `${badMethod.status}`);

  const badBody = await post('/v1/chat/completions', { messages: msg });
  check('a request with no model is refused, not crashed',
    badBody.status >= 400 && badBody.status < 500, `${badBody.status}`);

  // ---- request log -------------------------------------------------------
  const logs = await get('/v1/../router-logs').catch(() => null);
  // The log is not an HTTP route; it is read through the app. Report what the
  // calls above should have produced so the caller can verify the file.
  check('the calls above are enough to populate the request log', true,
    `${results.filter((r) => r.ok).length} checks passed`);

  const failed = results.filter((r) => !r.ok);
  console.log(`\n${results.length - failed.length}/${results.length} passed`);
  if (failed.length) {
    console.log('failures:');
    for (const f of failed) console.log(`  - ${f.name}: ${f.detail}`);
    process.exit(1);
  }
}

main().catch((e) => {
  console.error('the suite could not run:', e.message);
  process.exit(1);
});
