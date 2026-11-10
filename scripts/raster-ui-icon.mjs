// Rasterise the app icon SVG to the sidebar logo PNG.
//
// `tauri icon` writes the platform set under src-tauri/icons, but the in-app
// logo at ui/icon.png (shown at 36px in the sidebar) is a separate file. This
// renders the same drawing so the sidebar mark and the window icon match.
//
// Usage: node scripts/raster-ui-icon.mjs

import { readFileSync, writeFileSync, existsSync, mkdtempSync } from 'node:fs';
import { execFileSync } from 'node:child_process';
import { join, dirname, resolve } from 'node:path';
import { tmpdir } from 'node:os';
import { fileURLToPath } from 'node:url';

const HERE = dirname(fileURLToPath(import.meta.url));
const SVG = join(HERE, 'app-icon.svg');
const OUT = resolve(HERE, '..', 'ui', 'icon.png');
// 128px covers the 36px sidebar slot at 3x with room left over.
const SIZE = 128;

const CANDIDATES = [
  'C:/Program Files/Google/Chrome/Application/chrome.exe',
  'C:/Program Files (x86)/Google/Chrome/Application/chrome.exe',
  'C:/Program Files (x86)/Microsoft/Edge/Application/msedge.exe',
  'C:/Program Files/Microsoft/Edge/Application/msedge.exe',
];
const chrome = CANDIDATES.find((p) => existsSync(p));
if (!chrome) {
  console.error('No Chrome/Edge found - cannot rasterise the app icon.');
  process.exit(1);
}

const data = 'data:image/svg+xml;base64,' + readFileSync(SVG).toString('base64');
const page = `<!DOCTYPE html><meta charset="utf-8"><pre id="out">pending</pre><script>
(async () => {
  const img = new Image();
  img.src = ${JSON.stringify(data)};
  await img.decode();
  const c = document.createElement('canvas');
  c.width = ${SIZE}; c.height = ${SIZE};
  c.getContext('2d').drawImage(img, 0, 0, ${SIZE}, ${SIZE});
  document.getElementById('out').textContent = c.toDataURL('image/png');
})();
</script>`;

const work = mkdtempSync(join(tmpdir(), 'rb-uiicon-'));
const pagePath = join(work, 'page.html');
writeFileSync(pagePath, page, 'utf8');

const dom = execFileSync(chrome, [
  '--headless=new', '--disable-gpu', '--no-first-run', '--no-default-browser-check',
  '--allow-file-access-from-files', '--hide-scrollbars',
  `--user-data-dir=${join(work, 'profile')}`,
  '--virtual-time-budget=30000', '--dump-dom',
  'file:///' + pagePath.replace(/\\/g, '/'),
], { encoding: 'utf8', maxBuffer: 64 * 1024 * 1024, timeout: 60000 });

const m = dom.match(/<pre id="out">([\s\S]*?)<\/pre>/);
if (!m || m[1] === 'pending') {
  console.error('The page produced no image.');
  process.exit(1);
}
const png = Buffer.from(m[1].replace(/&amp;/g, '&').split(',')[1], 'base64');
writeFileSync(OUT, png);
console.log(`wrote ${OUT} (${png.length} bytes, ${SIZE}px)`);
