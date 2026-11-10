// Live checks for the AI Router's API-key rules.
//
// The router is a loopback endpoint that republishes paid model access, so the
// key rules are the security boundary: no key required from this machine when no
// key is configured, a key mandatory once one exists, a disabled key refused, and
// a restricted key limited to the models it names.
const ROUTER = process.env.ROUTER || 'http://127.0.0.1:11930';
const results = [];

function check(name, ok, detail) {
  results.push({ name, ok, detail });
  console.log(`${ok ? 'PASS' : 'FAIL'}  ${name}${detail ? `  — ${detail}` : ''}`);
}

async function call(headers, model = 'mock/mock-a') {
  const res = await fetch(ROUTER + '/v1/chat/completions', {
    method: 'POST',
    headers: { 'content-type': 'application/json', ...headers },
    body: JSON.stringify({ model, messages: [{ role: 'user', content: 'hi' }] }),
  });
  const text = await res.text();
  let json = null;
  try { json = JSON.parse(text); } catch (_) {}
  return { status: res.status, json };
}

async function models(headers) {
  const res = await fetch(ROUTER + '/v1/models', { headers });
  const text = await res.text();
  let json = null;
  try { json = JSON.parse(text); } catch (_) {}
  return { status: res.status, json };
}

async function main() {
  // ---- no key -----------------------------------------------------------
  const none = await call({});
  check('a loopback call with no key is refused once a key exists',
    none.status === 401, `${none.status} ${none.json?.error?.message?.slice(0, 50)}`);
  check('the refusal says how to send the key',
    /Bearer|x-api-key/.test(none.json?.error?.message || ''), none.json?.error?.message?.slice(0, 70));

  // ---- wrong key --------------------------------------------------------
  const wrong = await call({ authorization: 'Bearer sk-not-a-real-key' });
  check('an unknown key is refused', wrong.status === 401, `${wrong.status}`);

  // ---- disabled key -----------------------------------------------------
  const off = await call({ authorization: 'Bearer sk-rbtest-disabled-03' });
  check('a disabled key is refused', off.status === 401, `${off.status}`);

  // ---- valid key, bearer ------------------------------------------------
  const bearer = await call({ authorization: 'Bearer sk-rbtest-laptop-0001' });
  check('a valid key in Authorization: Bearer is accepted', bearer.status === 200, `${bearer.status}`);

  // ---- valid key, x-api-key ---------------------------------------------
  const xkey = await call({ 'x-api-key': 'sk-rbtest-laptop-0001' });
  check('the same key in x-api-key is accepted', xkey.status === 200, `${xkey.status}`);

  // ---- restricted key ---------------------------------------------------
  const allowed = await call({ authorization: 'Bearer sk-rbtest-mockonly-02' }, 'mock/mock-b');
  check('a restricted key may call a model it names', allowed.status === 200, `${allowed.status}`);

  const denied = await call({ authorization: 'Bearer sk-rbtest-mockonly-02' }, 'risun/bansos/deepseek-v4.1-flash-1037');
  check('a restricted key is refused on a model it does not name',
    denied.status === 403 || denied.status === 401,
    `${denied.status} ${denied.json?.error?.message?.slice(0, 60)}`);

  const deniedCombo = await call({ authorization: 'Bearer sk-rbtest-mockonly-02' }, 'combo/fallback-test');
  check('a restricted key cannot reach a combo it does not name',
    deniedCombo.status === 403 || deniedCombo.status === 401, `${deniedCombo.status}`);

  // ---- models listing under a restricted key ----------------------------
  const listed = await models({ authorization: 'Bearer sk-rbtest-mockonly-02' });
  const ids = (listed.json?.data || []).map((m) => m.id);
  check('the model list is filtered to what the key may use',
    ids.length > 0 && ids.every((id) => id.startsWith('mock/')), ids.join(', '));

  const full = await models({ authorization: 'Bearer sk-rbtest-laptop-0001' });
  const fullIds = (full.json?.data || []).map((m) => m.id);
  check('an unrestricted key sees every model',
    fullIds.length > ids.length, `${fullIds.length} vs ${ids.length}`);

  const failed = results.filter((r) => !r.ok);
  console.log(`\n${results.length - failed.length}/${results.length} passed`);
  if (failed.length) process.exit(1);
}

main().catch((e) => {
  console.error('the suite could not run:', e.message);
  process.exit(1);
});
