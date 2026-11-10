// Probes the rustbox MCP bridge directly, without the MCP shim.
//
// The shim is what a real client uses, but it hides the transport: it re-reads
// the endpoint file every call and turns a refused connection into a friendly
// message. That makes it useless for checking whether the bridge itself refuses
// a revoked token. This script speaks the line protocol directly so each
// security property can be asserted on its own.
//
//   node scripts/probe-mcp.js            # run every check
//   node scripts/probe-mcp.js <port> <token>   # talk to a specific endpoint

const net = require('net');
const fs = require('fs');
const os = require('os');
const path = require('path');

const endpointFile = path.join(
  process.env.LOCALAPPDATA || path.join(os.homedir(), 'AppData', 'Local'),
  'rustbox',
  'mcp-endpoint.json'
);

function readEndpoint() {
  const raw = fs.readFileSync(endpointFile, 'utf8');
  return JSON.parse(raw);
}

/** Send one request and resolve with the parsed reply, or a transport error. */
function request(port, payload, timeoutMs = 15000) {
  return new Promise((resolve) => {
    const socket = net.createConnection({ host: '127.0.0.1', port });
    let buf = '';
    let settled = false;
    const done = (v) => {
      if (settled) return;
      settled = true;
      socket.destroy();
      resolve(v);
    };
    socket.setTimeout(timeoutMs, () => done({ transport: 'timeout' }));
    socket.on('error', (e) => done({ transport: e.code || e.message }));
    socket.on('data', (d) => {
      buf += d.toString('utf8');
      const nl = buf.indexOf('\n');
      if (nl >= 0) {
        try {
          done({ reply: JSON.parse(buf.slice(0, nl)) });
        } catch (e) {
          done({ transport: `bad json: ${e.message}` });
        }
      }
    });
    socket.on('connect', () => socket.write(JSON.stringify(payload) + '\n'));
    socket.on('close', () => done({ transport: 'closed' }));
  });
}

const checks = [];
function check(name, ok, detail) {
  checks.push({ name, ok, detail });
  console.log(`${ok ? 'PASS' : 'FAIL'}  ${name}${detail ? ` — ${detail}` : ''}`);
}

(async () => {
  let ep;
  try {
    ep = readEndpoint();
  } catch (e) {
    console.error(`SKIP: no endpoint file at ${endpointFile} (${e.message})`);
    process.exit(2);
  }
  const port = Number(process.argv[2] || ep.port);
  const token = process.argv[3] || ep.token;

  const good = await request(port, { id: 1, token, method: 'list_tabs', params: {} });
  check('a valid token is served', !!(good.reply && good.reply.ok === true), good.transport || 'ok');

  const bad = await request(port, { id: 2, token: 'deadbeef'.repeat(4), method: 'list_tabs', params: {} });
  check(
    'a wrong token is refused',
    !!(bad.reply && bad.reply.ok === false && /token/i.test(bad.reply.error || '')),
    bad.reply ? bad.reply.error : bad.transport
  );

  const missing = await request(port, { id: 3, method: 'list_tabs', params: {} });
  check('a missing token is refused', !!(missing.reply && missing.reply.ok === false), missing.reply ? missing.reply.error : missing.transport);

  const unknown = await request(port, { id: 4, token, method: 'nope', params: {} });
  check('an unknown method is refused', !!(unknown.reply && unknown.reply.ok === false), unknown.reply ? unknown.reply.error : unknown.transport);

  const status = await request(port, { id: 5, token, method: 'status', params: {} });
  check('status reports the app pid', !!(status.reply && status.reply.ok && status.reply.result.pid > 0), status.reply ? `pid=${status.reply.result.pid}` : status.transport);

  const failed = checks.filter((c) => !c.ok).length;
  console.log(`\n${checks.length - failed}/${checks.length} checks passed`);
  process.exit(failed === 0 ? 0 : 1);
})();
