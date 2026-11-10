#!/usr/bin/env node
// Convert oversized catalog icons into small PNGs.
//
// 33 recipes ship an SVG that wraps a base64-encoded raster (a 1024x1024 PNG
// pasted into `<image href="data:...">`). They total 17.5 MB, and the biggest
// one — google-maps — is 4.2 MB for a 48px tile. Dropping them from the catalog
// was the old answer, which left those apps without an icon at all.
//
// Rasterising the SVG once at display size keeps the artwork and costs ~15 KB
// per icon instead of megabytes. Headless Chrome does the render (it is already
// a hard dependency of normalize-icons.mjs), and the result is written as PNG
// next to the other icons.
//
// Usage: node scripts/rasterize-icons.mjs <srcDir> <outDir> [--size=160]
//   <srcDir>  staging directory holding <id>.svg files to convert
//   <outDir>  ui/icons, where <id>.png is written

import { readFileSync, writeFileSync, readdirSync, existsSync, mkdtempSync } from 'node:fs';
import { execFileSync } from 'node:child_process';
import { join, dirname, resolve } from 'node:path';
import { tmpdir } from 'node:os';
import { fileURLToPath } from 'node:url';

const HERE = dirname(fileURLToPath(import.meta.url));

const args = process.argv.slice(2).filter(a => !a.startsWith('--'));
const sizeArg = process.argv.find(a => a.startsWith('--size='));
// 160px covers a 48px tile at 3x DPI with room for future zoom.
const SIZE = sizeArg ? parseInt(sizeArg.split('=')[1], 10) : 160;

const SRC = resolve(args[0] || join(HERE, '..', 'ui', 'icons-src'));
const OUT = resolve(args[1] || join(HERE, '..', 'ui', 'icons'));

const CANDIDATES = [
  'C:/Program Files/Google/Chrome/Application/chrome.exe',
  'C:/Program Files (x86)/Google/Chrome/Application/chrome.exe',
  'C:/Program Files (x86)/Microsoft/Edge/Application/msedge.exe',
  'C:/Program Files/Microsoft/Edge/Application/msedge.exe',
];
const chrome = CANDIDATES.find(p => existsSync(p));
if (!chrome) {
  console.error('No Chrome/Edge found - cannot rasterize icons.');
  process.exit(1);
}

if (!existsSync(SRC)) {
  // Nothing oversized this run. Not an error: the build must succeed either way.
  console.log('rasterize: no staging directory, nothing to do');
  process.exit(0);
}

const files = readdirSync(SRC).filter(f => f.toLowerCase().endsWith('.svg')).sort();
if (!files.length) {
  console.log('rasterize: staging directory is empty, nothing to do');
  process.exit(0);
}

const payload = files.map(f => ({
  id: f.replace(/\.svg$/i, ''),
  d: 'data:image/svg+xml;base64,' + readFileSync(join(SRC, f)).toString('base64'),
}));

// Rasterise in the browser: decode the SVG, paint it on a square canvas, and
// hand back PNG data URLs. A white-to-transparent check is not needed because
// the canvas starts cleared, so the SVG's own transparency survives.
const page = `<!DOCTYPE html><meta charset="utf-8"><pre id="out">pending</pre><script>
const icons = ${JSON.stringify(payload)};
const S = ${SIZE};
(async () => {
  const res = [];
  for (const it of icons) {
    const img = new Image();
    img.src = it.d;
    try { await img.decode(); } catch (e) { res.push({ id: it.id, error: 'decode' }); continue; }
    const c = document.createElement('canvas');
    c.width = S; c.height = S;
    const g = c.getContext('2d');
    // Contain, not stretch: an embedded raster may not be square.
    const ar = img.width && img.height ? img.width / img.height : 1;
    let w = S, h = S;
    if (ar > 1) { h = Math.round(S / ar); } else if (ar < 1) { w = Math.round(S * ar); }
    g.drawImage(img, Math.round((S - w) / 2), Math.round((S - h) / 2), w, h);
    res.push({ id: it.id, png: c.toDataURL('image/png') });
  }
  document.getElementById('out').textContent = JSON.stringify(res);
})();
</script>`;

const work = mkdtempSync(join(tmpdir(), 'rb-raster-'));
const pagePath = join(work, 'raster.html');
writeFileSync(pagePath, page, 'utf8');

let dom;
try {
  dom = execFileSync(chrome, [
    '--headless=new', '--disable-gpu', '--no-first-run', '--no-default-browser-check',
    '--allow-file-access-from-files', '--hide-scrollbars',
    `--user-data-dir=${join(work, 'profile')}`,
    '--virtual-time-budget=120000', '--dump-dom',
    'file:///' + pagePath.replace(/\\/g, '/'),
  ], { encoding: 'utf8', maxBuffer: 256 * 1024 * 1024, timeout: 300000 });
} catch (e) {
  console.error('Headless browser failed:', e.message);
  process.exit(1);
}

const m = dom.match(/<pre id="out">([\s\S]*?)<\/pre>/);
if (!m || m[1] === 'pending') {
  console.error('Rasterize page produced no result (headless run did not finish).');
  process.exit(1);
}
const json = m[1]
  .replace(/&lt;/g, '<').replace(/&gt;/g, '>')
  .replace(/&quot;/g, '"').replace(/&#39;/g, "'").replace(/&amp;/g, '&');

const results = JSON.parse(json);
let written = 0;
let bytes = 0;
const failed = [];

for (const r of results) {
  if (r.error || !r.png) { failed.push(`${r.id}: ${r.error || 'no png'}`); continue; }
  const buf = Buffer.from(r.png.split(',')[1], 'base64');
  writeFileSync(join(OUT, `${r.id}.png`), buf);
  written++;
  bytes += buf.length;
}

console.log(`rasterize: ${written}/${files.length} icons -> ${(bytes / 1024).toFixed(0)} KB (${SIZE}px)`);
if (failed.length) {
  console.log(`  failed: ${failed.join(', ')}`);
  process.exit(1);
}
