#!/usr/bin/env node
// Throwaway forward proxy that records every request it receives.
//
// Used to prove that a per-app proxy configured in the UI actually reaches the
// embedded WebView2 tab. The probe app points at a host that does not resolve,
// so the only way a request can appear in the log is by travelling through this
// proxy. It answers with a fixed page so the test needs no internet access.
//
// Usage: node scripts/proxy-probe.mjs [port] [logFile]
//        Prints "ready" once listening; stops on a signal.

import { createServer } from 'node:http';
import { appendFileSync, writeFileSync } from 'node:fs';

const port = Number(process.argv[2] || 8899);
const logFile = process.argv[3] || 'proxy-probe.log';

writeFileSync(logFile, '');

function record(entry) {
  appendFileSync(logFile, JSON.stringify(entry) + '\n');
}

const server = createServer((req, res) => {
  record({ at: Date.now(), kind: 'request', method: req.method, url: req.url, host: req.headers.host });
  const body = '<!doctype html><title>proxied</title><h1 id="x">PROXIED</h1>';
  res.writeHead(200, {
    'content-type': 'text/html; charset=utf-8',
    'content-length': Buffer.byteLength(body),
    'cache-control': 'no-store',
  });
  res.end(body);
});

// HTTPS arrives as CONNECT. Refusing the tunnel is enough: seeing the CONNECT
// proves the proxy was used, and answering it would need a TLS MITM the probe
// has no business doing.
server.on('connect', (req, socket) => {
  record({ at: Date.now(), kind: 'connect', target: req.url });
  socket.end('HTTP/1.1 502 probe refuses to tunnel\r\n\r\n');
});

server.listen(port, '127.0.0.1', () => {
  console.log('ready');
});
