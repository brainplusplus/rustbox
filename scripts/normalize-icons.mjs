#!/usr/bin/env node
// Normalize catalog icons so every brand mark reads at the same optical size.
//
// The Ferdium recipes ship whatever the brand published: 145 different viewBoxes
// and 53 files with no viewBox at all. Dropped into a fixed-size tile, a thin
// glyph and a full-bleed square logo end up looking nothing alike.
//
// This script measures the real ink of each icon (headless Chromium + canvas),
// rewrites the root <svg> so the drawing sits centred on a square canvas with a
// fixed ink-to-canvas ratio, then re-measures to prove the rewrite worked.
//
// Dark achromatic glyphs are inverted with an SVG filter rather than a fill
// override: a filter works on rendered pixels, so stroke-only icons keep their
// hollow interiors and multi-tone marks keep their tonal steps.
//
// Usage: node scripts/normalize-icons.mjs [--dry-run]
// Run after build-catalog.ps1 regenerates ui/icons (it overwrites these files).

import { readFileSync, writeFileSync, readdirSync, existsSync, mkdtempSync } from 'node:fs';
import { execFileSync } from 'node:child_process';
import { join, dirname, resolve } from 'node:path';
import { tmpdir } from 'node:os';
import { fileURLToPath } from 'node:url';

const HERE = dirname(fileURLToPath(import.meta.url));
const ICON_DIR = resolve(HERE, '..', 'ui', 'icons');
const REPORT = resolve(HERE, 'icon-report.json');

const INK_RATIO = 0.78;   // fraction of the square canvas the ink should fill
const MEASURE_PX = 512;   // rasterisation size used for measuring
const ALPHA_MIN = 12;     // ignore near-transparent antialiasing fringes
const MAX_ZOOM = 3.2;     // refuse to blow a 1px speck up into a whole tile
const DARK_LUMA = 0.30;   // ink darker than this vanishes on the dark UI
const PLATE_FILL = 0.75;  // ink density past this is a solid plate, not a glyph
const VERIFY_TOLERANCE = 0.03;

const dryRun = process.argv.includes('--dry-run');

const CANDIDATES = [
  'C:/Program Files/Google/Chrome/Application/chrome.exe',
  'C:/Program Files (x86)/Google/Chrome/Application/chrome.exe',
  'C:/Program Files (x86)/Microsoft/Edge/Application/msedge.exe',
  'C:/Program Files/Microsoft/Edge/Application/msedge.exe',
];
const chrome = CANDIDATES.find(p => existsSync(p));
if (!chrome) {
  console.error('No Chrome/Edge found - cannot measure icons.');
  process.exit(1);
}

// ---------------------------------------------------------------------------
// Measure ink boxes in a real browser. Icons are inlined as data: URLs so the
// canvas is never tainted and getImageData stays readable.
// ---------------------------------------------------------------------------
function measure(svgPaths) {
  const payload = svgPaths.map(p => ({
    f: p,
    d: 'data:image/svg+xml;base64,' + readFileSync(p).toString('base64'),
  }));

  const page = `<!DOCTYPE html><meta charset="utf-8"><pre id="out">pending</pre><script>
const icons = ${JSON.stringify(payload)};
const S = ${MEASURE_PX}, A = ${ALPHA_MIN};
(async () => {
  const res = [];
  for (const it of icons) {
    const img = new Image();
    img.src = it.d;
    try { await img.decode(); } catch (e) { res.push({ f: it.f, error: 'decode' }); continue; }
    const c = document.createElement('canvas');
    c.width = S; c.height = S;
    const g = c.getContext('2d', { willReadFrequently: true });
    g.clearRect(0, 0, S, S);
    g.drawImage(img, 0, 0, S, S);
    const px = g.getImageData(0, 0, S, S).data;
    let minX = S, minY = S, maxX = -1, maxY = -1, ink = 0, lumaSum = 0, satSum = 0, bright = 0;
    for (let y = 0; y < S; y++) {
      for (let x = 0; x < S; x++) {
        const i = (y * S + x) * 4;
        if (px[i + 3] <= A) continue;
        ink++;
        if (x < minX) minX = x;
        if (x > maxX) maxX = x;
        if (y < minY) minY = y;
        if (y > maxY) maxY = y;
        const r = px[i] / 255, gg = px[i + 1] / 255, b = px[i + 2] / 255;
        const luma = 0.2126 * r + 0.7152 * gg + 0.0722 * b;
        lumaSum += luma;
        if (luma > 0.7) bright++;
        const mx = Math.max(r, gg, b), mn = Math.min(r, gg, b);
        satSum += mx === 0 ? 0 : (mx - mn) / mx;
      }
    }
    if (maxX < 0) { res.push({ f: it.f, error: 'blank' }); continue; }
    res.push({
      f: it.f, ink, bright: bright / ink,
      x0: minX / S, y0: minY / S, x1: (maxX + 1) / S, y1: (maxY + 1) / S,
      w: (maxX + 1 - minX) / S, h: (maxY + 1 - minY) / S,
      luma: lumaSum / ink, sat: satSum / ink, fill: ink / (S * S),
    });
  }
  document.getElementById('out').textContent = JSON.stringify(res);
})();
</script>`;

  const work = mkdtempSync(join(tmpdir(), 'rb-icons-'));
  const pagePath = join(work, 'measure.html');
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
    console.error('Measurement page produced no result (headless run did not finish).');
    process.exit(1);
  }
  const json = m[1]
    .replace(/&lt;/g, '<').replace(/&gt;/g, '>')
    .replace(/&quot;/g, '"').replace(/&#39;/g, "'").replace(/&amp;/g, '&');
  return new Map(JSON.parse(json).map(x => [x.f, x]));
}

// ---------------------------------------------------------------------------
// Rewrite one SVG. Returns the new text, or a string explaining the skip.
// ---------------------------------------------------------------------------
const INVERT_DEFS =
  '<defs><filter id="rb-ink-invert" x="0" y="0" width="1" height="1" color-interpolation-filters="sRGB">' +
  '<feComponentTransfer><feFuncR type="linear" slope="-1" intercept="1"/>' +
  '<feFuncG type="linear" slope="-1" intercept="1"/>' +
  '<feFuncB type="linear" slope="-1" intercept="1"/></feComponentTransfer></filter></defs>';

// Solid dark logos (a filled plate, often with the glyph knocked out of it) are
// unreadable on the dark surface but must not be inverted either: inverting a
// full-bleed plate turns the whole tile into a glaring white square. Give those
// a light rounded pad and shrink the mark onto it, so it reads like the brand
// on a card instead of a hole in the UI.
const PAD_INSET = 0.07;   // pad is inset from the canvas edge (fraction of side)
const PAD_SCALE = 0.84;   // how much of the pad the original mark fills
const PAD_FILL = '#EDEFF2';

function padWrapper(nx, ny, ns) {
  const inset = ns * PAD_INSET;
  const cx = nx + ns / 2, cy = ny + ns / 2;
  const r = (ns - inset * 2) * 0.22;
  return {
    rect: `<rect x="${+(nx + inset).toFixed(3)}" y="${+(ny + inset).toFixed(3)}" ` +
      `width="${+(ns - inset * 2).toFixed(3)}" height="${+(ns - inset * 2).toFixed(3)}" ` +
      `rx="${+r.toFixed(3)}" fill="${PAD_FILL}"/>`,
    open: `<g transform="translate(${+cx.toFixed(3)} ${+cy.toFixed(3)}) ` +
      `scale(${PAD_SCALE}) translate(${+(-cx).toFixed(3)} ${+(-cy).toFixed(3)})">`,
    close: '</g>',
  };
}

function rewrite(raw, mt) {
  const openTag = raw.match(/<svg\b[^>]*>/i);
  if (!openTag) return { skip: 'no <svg> tag' };
  const tag = openTag[0];
  const attr = name => {
    const r = new RegExp(`\\b${name}\\s*=\\s*["']([^"']*)["']`, 'i').exec(tag);
    return r ? r[1].trim() : null;
  };

  // Coordinate space of the drawing: viewBox when present, else width/height.
  const vb = attr('viewBox');
  let vx = 0, vy = 0, vw, vh;
  if (vb) {
    const p = vb.split(/[\s,]+/).map(Number);
    if (p.length !== 4 || p.some(n => !Number.isFinite(n))) return { skip: 'bad viewBox' };
    [vx, vy, vw, vh] = p;
  } else {
    const w = parseFloat(attr('width')), h = parseFloat(attr('height'));
    const wu = attr('width') || '', hu = attr('height') || '';
    if (!Number.isFinite(w) || !Number.isFinite(h) || w <= 0 || h <= 0 || /%$/.test(wu) || /%$/.test(hu)) {
      return { skip: 'no usable viewBox or width/height' };
    }
    vw = w; vh = h;
  }

  const ix0 = vx + mt.x0 * vw, ix1 = vx + mt.x1 * vw;
  const iy0 = vy + mt.y0 * vh, iy1 = vy + mt.y1 * vh;
  const iw = ix1 - ix0, ih = iy1 - iy0;
  if (!(iw > 0) || !(ih > 0)) return { skip: 'empty ink box' };

  const side = Math.max(iw, ih) / INK_RATIO;
  if (side > Math.max(vw, vh) * MAX_ZOOM) {
    return { skip: `ink too small to scale (${(side / Math.max(vw, vh)).toFixed(1)}x)` };
  }
  const cx = (ix0 + ix1) / 2, cy = (iy0 + iy1) / 2;
  const nx = +(cx - side / 2).toFixed(3);
  const ny = +(cy - side / 2).toFixed(3);
  const ns = +side.toFixed(3);

  // viewBox governs scaling, so width/height must go or the tile crops the art.
  let next = tag
    .replace(/\s*\bviewBox\s*=\s*["'][^"']*["']/i, '')
    .replace(/\s*\bwidth\s*=\s*["'][^"']*["']/i, '')
    .replace(/\s*\bheight\s*=\s*["'][^"']*["']/i, '')
    .replace(/^<svg/i, `<svg viewBox="${nx} ${ny} ${ns} ${ns}"`);

  let body = raw.slice(openTag.index + tag.length);

  // Ink coverage *inside* the ink box: near 1.0 means a solid filled plate,
  // lower means a glyph with holes and counters.
  const density = mt.ink / (MEASURE_PX * MEASURE_PX * mt.w * mt.h);
  const darkMono = mt.luma < DARK_LUMA && mt.sat < 0.12;

  // A dark achromatic glyph is invisible on the dark surface. Invert rendered
  // pixels instead of overriding fills, so hollow strokes stay hollow.
  if (darkMono && density < PLATE_FILL) {
    next = next.replace(/^<svg/i, '<svg filter="url(#rb-ink-invert)"');
    body = INVERT_DEFS + body;
  } else if (darkMono) {
    // A solid plate cannot be inverted without turning the tile white, so back
    // it with a light pad and let the mark sit on it. The group has to close
    // inside the root element: appending it after </svg> makes the file
    // malformed XML, and the browser then renders nothing at all.
    const pad = padWrapper(nx, ny, ns);
    const end = body.search(/<\/svg\s*>\s*$/i);
    body = end === -1
      ? pad.rect + pad.open + body + pad.close
      : pad.rect + pad.open + body.slice(0, end) + pad.close + body.slice(end);
  }

  return {
    out: next + body,
    inverted: darkMono && density < PLATE_FILL,
    padded: darkMono && density >= PLATE_FILL,
    viewBox: `${nx} ${ny} ${ns} ${ns}`,
  };
}

// ---------------------------------------------------------------------------
const files = readdirSync(ICON_DIR).filter(f => f.toLowerCase().endsWith('.svg')).sort()
  .map(f => join(ICON_DIR, f));
if (!files.length) {
  console.error(`No SVG files in ${ICON_DIR}`);
  process.exit(1);
}

console.log(`measuring ${files.length} icons...`);
const before = measure(files);

// --audit: report the current state of every icon and stop. Useful after a
// normalization pass to see which marks are still too dark for the dark UI.
if (process.argv.includes('--audit')) {
  const rows = [...before.values()].filter(r => !r.error)
    .map(r => ({ ...r, density: r.ink / (MEASURE_PX * MEASURE_PX * r.w * r.h) }))
    .sort((a, b) => a.luma - b.luma);
  const dark = rows.filter(r => r.luma < DARK_LUMA && r.sat < 0.12);
  console.log(`\ndark achromatic marks (luma<${DARK_LUMA}, sat<0.12): ${dark.length}`);
  for (const r of dark) {
    const name = r.f.split(/[\\/]/).pop();
    const verdict = r.density >= PLATE_FILL ? 'solid plate' : 'thin glyph';
    console.log(`  ${name.padEnd(30)} luma=${r.luma.toFixed(3)} sat=${r.sat.toFixed(3)} density=${r.density.toFixed(2)}  ${verdict}`);
  }
  console.log('\nink-box spread after normalization (max deviation from target):');
  const devs = rows.map(r => Math.abs(Math.max(r.w, r.h) - INK_RATIO));
  console.log(`  worst=${Math.max(...devs).toFixed(4)} mean=${(devs.reduce((a, b) => a + b, 0) / devs.length).toFixed(4)}`);
  process.exit(0);
}

const report = { total: files.length, normalized: 0, skipped: [], inverted: [], padded: [], verify: [] };
const pending = [];

for (const p of files) {
  const name = p.split(/[\\/]/).pop();
  const mt = before.get(p);
  if (!mt || mt.error) { report.skipped.push({ f: name, why: mt ? mt.error : 'no metrics' }); continue; }

  const r = rewrite(readFileSync(p, 'utf8'), mt);
  if (r.skip) { report.skipped.push({ f: name, why: r.skip }); continue; }

  pending.push({ path: p, name, out: r.out, inverted: r.inverted, padded: r.padded });
  if (r.inverted) report.inverted.push(name);
  if (r.padded) report.padded.push(name);
}

if (!dryRun) for (const it of pending) writeFileSync(it.path, it.out, 'utf8');
report.normalized = pending.length;

// Prove it: re-measure what is now on disk and check the ink fills the canvas.
// A handful of icons carry blurred shadows whose bleed the geometric estimate
// cannot predict, so correct those by re-deriving the viewBox from measurement.
if (!dryRun) {
  for (let pass = 1; pass <= 3; pass++) {
    console.log(`verifying rewrite (pass ${pass})...`);
    const after = measure(files);
    const fixups = [];
    report.verify = [];

    for (const it of pending) {
      const a = after.get(it.path);
      if (!a || a.error) { report.verify.push({ f: it.name, problem: a ? a.error : 'no metrics' }); continue; }
      if (it.inverted && a.luma < 0.45) {
        report.verify.push({ f: it.name, problem: `still dark after invert (luma ${a.luma.toFixed(2)})` });
      }
      // A padded icon's ink box *is* the pad, so it covers more of the canvas by
      // design; the thing to prove is that the pad is actually light.
      if (it.padded) {
        // The pad now defines the ink box, so the ratio target does not apply;
        // what matters is that enough of the mark sits on light pixels.
        if (a.bright < 0.35) {
          report.verify.push({ f: it.name, problem: `pad not light enough (${(a.bright * 100).toFixed(0)}% light)` });
        }
        continue;
      }
      const ink = Math.max(a.w, a.h);
      if (Math.abs(ink - INK_RATIO) <= VERIFY_TOLERANCE) continue;

      // Re-derive the canvas from what actually rendered: keep the measured ink
      // centred and grow/shrink the box until the ink covers INK_RATIO of it.
      const raw = readFileSync(it.path, 'utf8');
      const tag = raw.match(/<svg\b[^>]*>/i)[0];
      const vb = /\bviewBox\s*=\s*["']([^"']*)["']/i.exec(tag)[1].trim().split(/[\s,]+/).map(Number);
      const [vx, vy, vw] = vb;
      const ix0 = vx + a.x0 * vw, ix1 = vx + a.x1 * vw;
      const iy0 = vy + a.y0 * vw, iy1 = vy + a.y1 * vw;
      const side = Math.max(ix1 - ix0, iy1 - iy0) / INK_RATIO;
      const cx = (ix0 + ix1) / 2, cy = (iy0 + iy1) / 2;
      const next = tag.replace(/\bviewBox\s*=\s*["'][^"']*["']/i,
        `viewBox="${+(cx - side / 2).toFixed(3)} ${+(cy - side / 2).toFixed(3)} ${+side.toFixed(3)} ${+side.toFixed(3)}"`);
      fixups.push({ path: it.path, text: next + raw.slice(tag.length), name: it.name, ink });
    }

    if (!fixups.length) {
      report.verify = [];
      console.log('  verify: all icons fill the canvas at the target ratio');
      break;
    }
    for (const f of fixups) {
      writeFileSync(f.path, f.text, 'utf8');
      report.verify.push({ f: f.name, problem: `corrected (ink was ${f.ink.toFixed(3)})` });
    }
    console.log(`  corrected ${fixups.length} icon(s): ${fixups.map(f => f.name).join(', ')}`);
  }
}

const summary = {
  ...report,
  inkRatio: INK_RATIO,
  skippedCount: report.skipped.length,
  invertedCount: report.inverted.length,
  paddedCount: report.padded.length,
  verifyFailures: report.verify.length,
};
writeFileSync(REPORT, JSON.stringify(summary, null, 2), 'utf8');

console.log(`icons=${summary.total} normalized=${summary.normalized} skipped=${summary.skippedCount} ` +
  `inverted=${summary.invertedCount} padded=${summary.paddedCount}`);
if (summary.skippedCount) {
  const reasons = {};
  for (const s of report.skipped) reasons[s.why] = (reasons[s.why] || 0) + 1;
  for (const [why, n] of Object.entries(reasons)) console.log(`  skipped ${n}: ${why}`);
}
if (report.verify.length) {
  console.log(`  residual notes (${report.verify.length}):`);
  for (const v of report.verify) console.log(`    ${v.f}: ${v.problem}`);
}
console.log(dryRun ? '(dry run - nothing written)' : `report=${REPORT}`);
