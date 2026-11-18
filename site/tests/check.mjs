#!/usr/bin/env node
// Standalone acceptance harness for the wamt landing page.
// It serves the built site from a free loopback port, drives Chromium with
// Playwright, captures the review screenshots, checks the acceptance list and
// runs a mobile Lighthouse pass. Tooling is installed into a temp prefix, never
// into the repo, and no user configuration files are written.
//
// Direct use is expected after the lead has built site/. See --help.

import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import http from 'node:http';
import zlib from 'node:zlib';
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { createRequire } from 'node:module';
import { fileURLToPath, pathToFileURL } from 'node:url';

const DEFAULT_SITE = fileURLToPath(new URL('../', import.meta.url));
const DEFAULT_OUT = 'D:/Projects/wamt/hq/assets/landing/screens/r2';
const DEFAULT_DEPS = path.join(os.tmpdir(), 'wamt-landing-check-deps');
const VIEWPORTS = [
  { name: '390x844', width: 390, height: 844, isMobile: true, hasTouch: true },
  { name: '1440x900', width: 1440, height: 900, isMobile: false, hasTouch: false }
];
const THEMES = ['light', 'dark'];
const POINTS = ['hero', 'mid-bite', 'evidence', 'suspects', 'case-notes', 'install'];
const HERO = '[data-scene="hero"]';
const HERO_BLOBS = HERO + ' :is([data-sprite="coral"], [data-sprite="lilac"], [data-sprite="blue"])';

function parseArgs(argv) {
  const out = { _flags: new Set() };
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (a.startsWith('--')) {
      const eq = a.indexOf('=');
      const key = eq === -1 ? a.slice(2) : a.slice(2, eq);
      if (eq !== -1) {
        out[key] = a.slice(eq + 1);
      } else if (['no-lighthouse', 'help', 'quiet'].includes(key)) {
        out._flags.add(key);
      } else if (i + 1 < argv.length && !argv[i + 1].startsWith('--')) {
        out[key] = argv[++i];
      } else {
        out[key] = true;
      }
    }
  }
  return out;
}

function usage() {
  return [
    'Usage: node site/tests/check.mjs [options]',
    '',
    'Options:',
    '  --out DIR          Screenshot output directory (default ' + DEFAULT_OUT + ')',
    '  --site DIR         Built site root (default the parent of tests/)',
    '  --deps DIR         Temp prefix for playwright/lighthouse (default ' + DEFAULT_DEPS + ')',
    '  --chrome FILE      Use an installed Chrome instead of downloading Chromium',
    '  --mid-scroll N     Force the scroll Y used for the mid-bite screenshot',
    '  --lh-json FILE     Save the full Lighthouse LHR as JSON here',
    '  --no-lighthouse    Skip the Lighthouse pass (faster local validation)',
    '  --quiet            Reduce progress logging',
    '  --help             Show this text',
    '',
    'Selector assumptions (from the landing brief):',
    '  .scene[data-scene=hero|stage|finale] with .sprite[data-sprite=coral|lilac|blue|plate|cookie][data-frame]',
    '  [data-beat=0..7] = hero, #facts, #live, #limits, #history, #saving, #alerts, #install',
    '  .eating-stage, [data-plate-meter], #evidence, #suspects, #install, #evidence-dialog',
    '  [data-exhibit], [data-exhibit-close], [data-copy], [data-copy-status], [data-theme-toggle], [data-menu-toggle]',
    '',
    'Exit: 0 all required checks pass, 1 a check failed, 2 setup error.'
  ].join('\n');
}

const ARGS = parseArgs(process.argv.slice(2));
if (ARGS._flags.has('help')) {
  console.log(usage());
  process.exit(0);
}

const SITE_ROOT = path.resolve(String(ARGS.site || DEFAULT_SITE));
const OUT_DIR = path.resolve(String(ARGS.out || DEFAULT_OUT));
const DEPS_ROOT = path.resolve(String(ARGS.deps || DEFAULT_DEPS));
const CHROME = ARGS.chrome && path.resolve(String(ARGS.chrome));
const BROWSERS_PATH = path.join(DEPS_ROOT, 'browsers');
const MID_SCROLL = ARGS['mid-scroll'] != null ? Number(ARGS['mid-scroll']) : null;
const LH_JSON = ARGS['lh-json'] ? String(ARGS['lh-json']) : null;
const NO_LH = ARGS._flags.has('no-lighthouse');
const QUIET = ARGS._flags.has('quiet');

function log(...parts) {
  if (!QUIET) console.log('[check]', ...parts);
}

function stamp() {
  return new Date().toISOString();
}

function walk(root) {
  const found = [];
  const stack = [root];
  while (stack.length) {
    const dir = stack.pop();
    let entries = [];
    try { entries = fs.readdirSync(dir, { withFileTypes: true }); } catch { continue; }
    for (const e of entries) {
      const p = path.join(dir, e.name);
      if (e.isDirectory()) stack.push(p);
      else found.push(p);
    }
  }
  return found;
}

function ensureDeps() {
  fs.mkdirSync(DEPS_ROOT, { recursive: true });
  fs.mkdirSync(BROWSERS_PATH, { recursive: true });
  process.env.PLAYWRIGHT_BROWSERS_PATH = BROWSERS_PATH;
  const req = createRequire(path.join(DEPS_ROOT, '__wamt_placeholder__.cjs'));
  const wanted = ['playwright'];
  if (!NO_LH) wanted.push('lighthouse', 'chrome-launcher');
  const missing = wanted.filter((m) => {
    try { req.resolve(m); return false; } catch { return true; }
  });
  if (missing.length) {
    log('installing temp tooling into', DEPS_ROOT + ':', missing.join(', '));
    const npm = process.platform === 'win32' ? 'npm.cmd' : 'npm';
    const r = spawnSync(npm, ['install', '--prefix', DEPS_ROOT, '--no-audit', '--no-fund', ...missing], {
      stdio: 'inherit',
      shell: process.platform === 'win32'
    });
    if (r.status !== 0) throw new Error('temp tooling install failed with status ' + r.status);
  }
  const pw = req('playwright');
  const exe = CHROME || pw.chromium.executablePath();
  if (CHROME && !fs.existsSync(CHROME)) throw new Error('Chrome executable not found: ' + CHROME);
  if (!fs.existsSync(exe)) {
    log('installing chromium into', BROWSERS_PATH);
    const cli = path.join(path.dirname(req.resolve('playwright/package.json')), 'cli.js');
    const r = spawnSync(process.execPath, [cli, 'install', 'chromium'], {
      stdio: 'inherit',
      env: { ...process.env, PLAYWRIGHT_BROWSERS_PATH: BROWSERS_PATH }
    });
    if (r.status !== 0) throw new Error('chromium install failed with status ' + r.status);
  }
  return { req, chromium: pw.chromium, chromiumExe: exe };
}

const MIME = {
  '.html': 'text/html; charset=utf-8',
  '.css': 'text/css; charset=utf-8',
  '.js': 'text/javascript; charset=utf-8',
  '.mjs': 'text/javascript; charset=utf-8',
  '.json': 'application/json; charset=utf-8',
  '.svg': 'image/svg+xml',
  '.png': 'image/png',
  '.jpg': 'image/jpeg',
  '.webp': 'image/webp',
  '.avif': 'image/avif',
  '.woff2': 'font/woff2',
  '.txt': 'text/plain; charset=utf-8',
  '.ico': 'image/x-icon',
  '.xml': 'application/xml; charset=utf-8'
};

function startServer(root) {
  const configPath = path.join(root, 'vercel.json');
  const rules = fs.existsSync(configPath) ? JSON.parse(fs.readFileSync(configPath, 'utf8')).headers || [] : [];
  return new Promise((resolve, reject) => {
    const server = http.createServer((req, res) => {
      let pathname;
      try { pathname = decodeURIComponent(new URL(req.url, 'http://127.0.0.1').pathname); }
      catch { res.writeHead(400); res.end('bad request'); return; }
      if (pathname === '/') pathname = '/index.html';
      if (pathname.split(/[\\/]/).some((part) => part.startsWith('.')) || pathname.startsWith('/tests/')) {
        res.writeHead(404); res.end('not found'); return;
      }
      const file = path.resolve(root, '.' + pathname);
      if (!file.startsWith(root + path.sep) && file !== path.join(root, 'index.html')) {
        res.writeHead(403); res.end('forbidden'); return;
      }
      fs.readFile(file, (err, data) => {
        if (err) { res.writeHead(404); res.end('not found'); return; }
        const type = MIME[path.extname(file).toLowerCase()] || 'application/octet-stream';
        const headers = {
          'Content-Type': type,
          'Cache-Control': 'no-store',
          'X-Content-Type-Options': 'nosniff'
        };
        for (const rule of rules) {
          const prefix = rule.source.replace(/\(\.\*\)$/, '');
          if (!pathname.startsWith(prefix)) continue;
          for (const header of rule.headers) headers[header.key] = header.value;
        }
        if (type.startsWith('text/') && /\bgzip\b/.test(req.headers['accept-encoding'] || '')) {
          data = zlib.gzipSync(data);
          headers['Content-Encoding'] = 'gzip';
          headers.Vary = 'Accept-Encoding';
        }
        res.writeHead(200, { ...headers, 'Content-Length': data.length });
        res.end(data);
      });
    });
    server.on('error', reject);
    server.listen(0, '127.0.0.1', () => {
      resolve({ server, port: server.address().port });
    });
  });
}

async function settle(page) {
  await page.evaluate(async () => {
    await document.fonts.ready;
    await new Promise((done) => requestAnimationFrame(() => requestAnimationFrame(done)));
    const visible = [...document.images].filter((img) => {
      const bounds = img.getBoundingClientRect();
      return bounds.width > 0 && bounds.bottom > 0 && bounds.top < innerHeight;
    });
    await Promise.all(visible.map((img) => img.decode()));
  });
}

async function goto(page, url) {
  await page.goto(url, { waitUntil: 'load', timeout: 30000 });
  await settle(page);
}

async function scrollY(page, y) {
  await page.evaluate((top) => window.scrollTo({ top, behavior: 'instant' }), Math.max(0, Math.round(y)));
  await settle(page);
}

async function scrollToSelector(page, selector) {
  await page.evaluate((sel) => {
    const el = document.querySelector(sel);
    if (el && el.scrollIntoView) el.scrollIntoView({ block: 'start', behavior: 'instant' });
  }, selector);
  await settle(page);
}

async function sectionScreenshot(page, section, file) {
  const navigation = page.locator('.site-header, .skip-link');
  const previous = await navigation.evaluateAll(nodes => nodes.map(node => {
    const visibility = node.style.visibility;
    node.style.visibility = 'hidden';
    return visibility;
  }));
  try {
    await section.screenshot({ path: file });
  } finally {
    await navigation.evaluateAll((nodes, values) => {
      nodes.forEach((node, i) => { node.style.visibility = values[i]; });
    }, previous);
  }
}

async function heroFrames(page) {
  return page.locator(HERO_BLOBS).evaluateAll(blobs => blobs.map(blob => Number(blob.dataset.frame)));
}

async function checkHeroPixels(page) {
  await page.locator(HERO).evaluate(async (scene) => {
    const border = scene.getBoundingClientRect();
    const edge = scene.querySelector('.cam-counter').getBoundingClientRect().top;
    const sprites = Array.from(scene.querySelectorAll('[data-sprite]'));
    const canvas = document.createElement('canvas');
    const ctx = canvas.getContext('2d', { willReadFrequently: true });
    const areas = [];
    for (const sprite of sprites) {
      const name = sprite.dataset.sprite;
      const body = !['plate', 'cookie'].includes(name);
      const style = getComputedStyle(sprite);
      const px = Number(style.getPropertyValue('--magnification'));
      const width = parseInt(style.getPropertyValue('--cell-w'), 10);
      const height = parseInt(style.getPropertyValue('--cell-h'), 10);
      const box = sprite.getBoundingClientRect();
      const visibleRows = body ? Math.min(height, Math.floor((edge - box.top) / px)) : height;
      const frame = Number(sprite.dataset.frame || style.getPropertyValue('--still'));
      const image = new Image();
      image.src = style.backgroundImage.slice(5, -2);
      await image.decode();
      canvas.width = width;
      canvas.height = height;
      ctx.drawImage(image, frame * width, 0, width, height, 0, 0, width, height);
      const { data } = ctx.getImageData(0, 0, width, visibleRows);
      const ink = new Set();
      let left = width, top = height, right = 0, bottom = 0;
      for (let y = 0; y < visibleRows; y++) {
        for (let x = 0; x < width; x++) {
          if (!data[(y * width + x) * 4 + 3]) continue;
          ink.add(y * width + x);
          left = Math.min(left, x);
          right = Math.max(right, x + 1);
          top = Math.min(top, y);
          bottom = Math.max(bottom, y + 1);
        }
      }
      const area = {
        name, body, left: box.left + left * px, right: box.left + right * px,
        top: box.top + top * px, bottom: box.top + bottom * px
      };
      if (!ink.size || area.left < border.left || area.right > border.right ||
          area.top < border.top || area.bottom > border.bottom) {
        throw new Error(name + ' has missing or clipped art');
      }
      if (body) {
        const first = ink.values().next().value;
        ink.delete(first);
        const pending = [first];
        while (pending.length) {
          const pixel = pending.pop();
          const x = pixel % width;
          const neighbors = [pixel - width, pixel + width];
          if (x > 0) neighbors.push(pixel - 1);
          if (x + 1 < width) neighbors.push(pixel + 1);
          for (const neighbor of neighbors) if (ink.delete(neighbor)) pending.push(neighbor);
        }
        if (ink.size || bottom !== visibleRows) {
          throw new Error(name + ' has floating pixels at frame ' + frame);
        }
      }
      for (const previous of areas) {
        if ((body || previous.body) && area.left < previous.right && area.right > previous.left &&
            area.top < previous.bottom && area.bottom > previous.top) {
          throw new Error(name + ' overlaps ' + previous.name);
        }
      }
      areas.push(area);
    }
  });
}

async function captureHero(page, url, save) {
  const now = new Date();
  await page.clock.install({ time: now });
  await page.clock.pauseAt(new Date(+now + 1000));
  await page.goto(url, { waitUntil: 'load' });
  await page.clock.runFor(220);
  assert.equal(await page.locator(HERO).getAttribute('data-pose'), 'hidden');
  assert.deepEqual(await heroFrames(page), [0, 0, 0]);
  await checkHeroPixels(page);
  await save('hero-hidden');

  await page.clock.runFor(230);
  await page.clock.resume();
  await settle(page);
  assert.equal(await page.locator(HERO).getAttribute('data-pose'), 'peek');
  assert.deepEqual(await heroFrames(page), [1, 1, 1]);
  await checkHeroPixels(page);
  await save('hero-peek');

  const box = await page.locator(HERO).boundingBox();
  const x = box.x + box.width / 2;
  const y = box.y + box.height / 2;
  for (const [dx, dy, frame] of [[-90, 0, 2], [0, -90, 4], [0, 90, 5], [90, 0, 3]]) {
    await page.mouse.move(x + dx, y + dy);
    await settle(page);
    assert.deepEqual(await heroFrames(page), [frame, frame, frame]);
    await checkHeroPixels(page);
  }
  await save('hero-gaze');

  for (const [offset, frame] of [[13, 6], [52, 7], [88, 8], [52, 7], [13, 6], [0, 3]]) {
    await scrollY(page, offset);
    assert.deepEqual(await heroFrames(page), [frame, frame, frame]);
    await checkHeroPixels(page);
    if (offset === 88) {
      assert.equal(await page.locator(HERO).getAttribute('data-pose'), 'sneak');
      await save('hero-sneak');
    }
  }
  const clock = page.locator('[data-cam-clock]');
  const time = await clock.textContent();
  assert.match(time, /^\d{2}:\d{2}:\d{2}$/);
  await page.waitForFunction(last => document.querySelector('[data-cam-clock]').dateTime !== last, time);
}

async function findMidScroll(page) {
  if (MID_SCROLL != null && Number.isFinite(MID_SCROLL)) return MID_SCROLL;
  const beats = await page.$$('[data-beat="3"], [data-beat="4"], [data-beat="5"]');
  for (const beat of beats) {
    for (const frac of [0.52, 0.55, 0.48]) {
      const y = await beat.evaluate((el, f) => {
        const r = el.getBoundingClientRect();
        return window.scrollY + r.top - window.innerHeight * f;
      }, frac);
      await scrollY(page, y);
      const ok = await page.evaluate(() => {
        const stage = document.querySelector('.eating-stage');
        if (!stage) return false;
        const r = stage.getBoundingClientRect();
        if (!(r.top < window.innerHeight && r.bottom > 0)) return false;
        const cookie = Number(stage.querySelector('[data-sprite="cookie"]').dataset.frame);
        const chomp = Number(stage.dataset.biteFrame);
        return cookie >= 1 && cookie <= 6 && chomp >= 4 && chomp <= 9;
      });
      if (ok) return Math.max(0, Math.round(y));
    }
  }
  throw new Error('Could not find a visible bite in progress. Use --mid-scroll to set the capture offset.');
}

async function main() {
  if (!fs.existsSync(SITE_ROOT)) {
    console.error('[check] site root not found: ' + SITE_ROOT);
    console.error('[check] build the site first, or point --site at a fixture.');
    process.exit(2);
  }
  fs.mkdirSync(OUT_DIR, { recursive: true });

  const tooling = ensureDeps();
  const { server, port } = await startServer(SITE_ROOT);
  const origin = 'http://127.0.0.1:' + port;
  const url = origin + '/';
  log('server start', stamp(), origin, 'root', SITE_ROOT);

  const results = [];
  const violations = [];
  const consoleErrors = [];
  const consoleWarnings = [];
  const pageErrors = [];
  const screenshots = [];
  const notes = [];
  let transfer = null;
  let lighthouseResult = null;
  let browser = null;

  function withTimeout(promise, ms, label) {
    let timer;
    const guard = new Promise((_, reject) => {
      timer = setTimeout(() => reject(new Error('timeout after ' + ms + 'ms in ' + label)), ms);
    });
    return Promise.race([promise, guard]).finally(() => clearTimeout(timer));
  }

  async function check(name, fn) {
    const t0 = Date.now();
    log('check begin', name);
    try {
      const detail = await withTimeout(fn(), 90000, name);
      const skipped = detail && typeof detail === 'object' && detail.skip;
      results.push({
        name,
        status: skipped ? 'SKIP' : 'PASS',
        detail: typeof detail === 'string' ? detail : (detail && detail.detail) || '',
        ms: Date.now() - t0
      });
      log('check end', name, skipped ? 'SKIP' : 'PASS', (Date.now() - t0) + 'ms');
    } catch (e) {
      results.push({ name, status: 'FAIL', detail: String((e && e.message) || e), ms: Date.now() - t0 });
      log('check end', name, 'FAIL', (Date.now() - t0) + 'ms');
    }
  }

  function makeMonitors(page, label) {
    page.setDefaultTimeout(15000);
    page.setDefaultNavigationTimeout(30000);
    page.on('console', (m) => {
      const t = m.type();
      const text = label + ': ' + m.text();
      if (t === 'error') consoleErrors.push(text);
      else if (t === 'warning') consoleWarnings.push(text);
    });
    page.on('pageerror', (e) => pageErrors.push(label + ': ' + (e.stack || e.message || String(e))));
    page.on('request', (r) => {
      const u = r.url();
      if (/^(data|blob|about|chrome-extension|devtools):/.test(u)) return;
      try {
        if (new URL(u).origin !== origin) violations.push({ label, url: u });
      } catch { /* ignore malformed */ }
    });
    page.route('**/*', (route) => {
      const u = route.request().url();
      if (/^(data|blob|about|chrome-extension|devtools):/.test(u)) return route.continue();
      try {
        if (new URL(u).origin !== origin) {
          violations.push({ label, url: u });
          return route.abort();
        }
      } catch { /* ignore malformed */ }
      return route.continue();
    });
  }

  try {
    browser = await tooling.chromium.launch({
      headless: true,
      executablePath: tooling.chromiumExe,
      args: ['--no-sandbox', '--disable-background-networking', '--disable-default-apps']
    });

    // Two viewports and two themes, with an additional desktop camera sequence.
    for (const vp of VIEWPORTS) {
      for (const theme of THEMES) {
        const ctx = await browser.newContext({
          viewport: { width: vp.width, height: vp.height },
          colorScheme: theme,
          reducedMotion: 'no-preference',
          isMobile: vp.isMobile,
          hasTouch: vp.hasTouch,
          deviceScaleFactor: 1
        });
        try {
          const page = await ctx.newPage();
          makeMonitors(page, 'shot-' + vp.name + '-' + theme);
          if (vp.isMobile) await goto(page, url);
          else await captureHero(page, url, async (state) => {
            const file = path.join(OUT_DIR, vp.width + '-' + theme + '-' + state + '.png');
            await page.screenshot({ path: file });
            screenshots.push(file);
          });
          const midY = await findMidScroll(page);

          for (const point of POINTS) {
            if (point === 'hero') {
              await scrollY(page, 0);
            } else if (point === 'mid-bite') {
              await scrollY(page, midY);
            } else {
              await scrollToSelector(page, '#' + point);
            }
            await settle(page);
            const name = vp.width + '-' + theme + '-' + (point === 'install' ? 'finale' : point) + '.png';
            const file = path.join(OUT_DIR, name);
            if (point === 'evidence') {
              const el = await page.$('#evidence');
              if (el) {
                await el.$$eval('img', async (images) => {
                  for (const image of images) image.loading = 'eager';
                  await Promise.all(images.map((image) => image.decode()));
                });
                await sectionScreenshot(page, page.locator('#evidence'), file);
              }
              else {
                await page.screenshot({ path: file });
                notes.push('missing #evidence, used viewport capture for ' + name);
              }
            } else if (['install', 'suspects', 'case-notes'].includes(point)) {
              await sectionScreenshot(page, page.locator('#' + point), file);
            } else {
              await page.screenshot({ path: file });
            }
            screenshots.push(file);
          }
          log('captured', vp.name, theme, 'midY', midY);
        } finally {
          await ctx.close();
        }
      }
    }

    // Reduced-motion finale.
    log('screenshot matrix captured, starting reduced-motion finale');
    {
      const ctx = await browser.newContext({
        viewport: { width: 390, height: 844 },
        colorScheme: 'dark',
        reducedMotion: 'reduce',
        deviceScaleFactor: 1
      });
      try {
        const page = await ctx.newPage();
        makeMonitors(page, 'shot-reduced-motion');
        await goto(page, url);
        await scrollToSelector(page, '#install');
        const file = path.join(OUT_DIR, '390-dark-reduced-finale.png');
        await sectionScreenshot(page, page.locator('#install'), file);
        screenshots.push(file);
      } finally {
        await ctx.close();
      }
    }

    // Functional context: desktop, JavaScript on, clipboard allowed.
    log('reduced-motion finale captured, starting functional checks');
    const ctxDesktop = await browser.newContext({
      viewport: { width: 1440, height: 900 },
      colorScheme: 'light',
      reducedMotion: 'no-preference',
      deviceScaleFactor: 1
    });
    await ctxDesktop.grantPermissions(['clipboard-read', 'clipboard-write'], { origin }).catch(() => {});

    await check('screenshot matrix includes both hero sequences', async () => {
      assert.equal(screenshots.length, 33, 'expected 33 screenshots, got ' + screenshots.length);
      const missing = screenshots.filter((f) => !fs.existsSync(f) || fs.statSync(f).size < 100);
      assert.equal(missing.length, 0, 'missing or empty screenshots: ' + missing.join(', '));
      return screenshots.length + ' files written to ' + OUT_DIR;
    });

    await check('transfer size, cold cache', async () => {
      const page = await ctxDesktop.newPage();
      makeMonitors(page, 'transfer');
      const cdp = await ctxDesktop.newCDPSession(page);
      await cdp.send('Network.enable');
      await cdp.send('Network.setCacheDisabled', { cacheDisabled: true });
      let encoded = 0;
      cdp.on('Network.loadingFinished', (p) => { encoded += p.encodedDataLength || 0; });
      await goto(page, url);
      const timing = await page.evaluate(() => {
        const all = performance.getEntriesByType('resource').concat(performance.getEntriesByType('navigation'));
        return all.reduce((sum, e) => sum + (e.transferSize || 0), 0);
      });
      transfer = { cdpEncodedBytes: encoded, resourceTimingBytes: timing };
      const best = encoded || timing;
      assert(best > 0, 'no transfer measured');
      assert(best < 1024 * 1024, 'transfer ' + best + ' bytes exceeds the 1 MB budget');
      await page.close();
      return 'cdp=' + encoded + 'B, resourceTiming=' + timing + 'B';
    });

    await check('gzipped source budget', async () => {
      const assets = walk(SITE_ROOT).filter((file) =>
        !path.relative(SITE_ROOT, file).split(path.sep).some((part) => part.startsWith('.') || part === 'tests'));
      const total = assets.reduce((sum, file) => sum + fs.statSync(file).size, 0);
      const files = assets.filter((file) => /\.(js|css|html)$/i.test(file));
      let raw = 0;
      let gz = 0;
      let jsGz = 0;
      for (const f of files) {
        const buf = fs.readFileSync(f);
        const g = zlib.gzipSync(buf, { level: 9 }).length;
        raw += buf.length;
        gz += g;
        if (/\.js$/i.test(f)) jsGz += g;
      }
      transfer = transfer || {};
      transfer.sourceRawBytes = raw;
      transfer.sourceGzipBytes = gz;
      transfer.jsGzipBytes = jsGz;
      transfer.siteBytes = total;
      assert(total < 1_000_000, 'static assets exceed 1 MB: ' + total + 'B');
      assert(files.length > 0, 'no html/css/js source found under ' + SITE_ROOT);
      assert(gz < 60 * 1024, 'html+css+js gzip ' + gz + 'B exceeds 60 KB');
      assert(jsGz < 25_000, 'JS gzip exceeds 25 KB: ' + jsGz + 'B');
      return 'site=' + total + 'B, gzip=' + gz + 'B, jsGzip=' + jsGz + 'B';
    });

    await check('markup contract', async () => {
      const page = await ctxDesktop.newPage();
      makeMonitors(page, 'markup');
      await goto(page, url);
      const info = await page.evaluate(() => {
        const scenes = Array.from(document.querySelectorAll('.scene[data-scene]')).map((e) => e.dataset.scene);
        const sprites = Array.from(document.querySelectorAll('.sprite[data-sprite]')).map((e) => e.dataset.sprite);
        const beats = Array.from(document.querySelectorAll('[data-beat]')).map((e) => e.dataset.beat).sort();
        return {
          scenes,
          sprites,
          beats,
          stage: !!document.querySelector('.eating-stage'),
          meter: !!document.querySelector('[data-plate-meter]')
        };
      });
      for (const s of ['hero', 'stage', 'finale']) assert(info.scenes.includes(s), 'missing scene ' + s);
      for (const s of ['coral', 'lilac', 'blue', 'plate', 'cookie']) assert(info.sprites.includes(s), 'missing sprite ' + s);
      assert.equal(info.beats.join(','), '0,1,2,3,4,5,6,7', 'beats must be 0..7, got ' + info.beats.join(','));
      assert(info.stage, 'missing .eating-stage');
      assert(info.meter, 'missing [data-plate-meter]');
      await page.close();
      return 'scenes=' + info.scenes.join('/') + ', sprites=' + info.sprites.length + ', beats=8';
    });

    await check('interactive controls present', async () => {
      const page = await ctxDesktop.newPage();
      makeMonitors(page, 'controls');
      await goto(page, url);
      const missing = await page.evaluate(() => {
        const sels = ['#evidence-dialog', '[data-exhibit]', '[data-exhibit-close]', '[data-copy]',
          '[data-copy-status]', '[data-theme-toggle]', '[data-menu-toggle]', '#evidence', '#suspects', '#install'];
        return sels.filter((s) => !document.querySelector(s));
      });
      assert.equal(missing.length, 0, 'missing controls: ' + missing.join(', '));
      await page.close();
      return 'all required selectors found';
    });

    await check('case notes and source metadata', async () => {
      const page = await ctxDesktop.newPage();
      makeMonitors(page, 'case-notes');
      await goto(page, url);
      const steps = await page.locator('#case-notes li').allTextContents();
      assert.equal(steps.length, 3);
      assert.match(steps[0], /T3 Code, Claude Code and Codex/);
      assert.match(steps[1], /One Rust binary/);
      assert.match(steps[2], /127\.0\.0\.1/);
      const alibis = await page.locator('.suspect-alibi').allTextContents();
      assert.deepEqual(alibis, ['Read-only SQLite state.', 'Transcripts in ~/.claude/projects.', 'Session files in ~/.codex/sessions.']);
      const source = JSON.parse(await page.locator('head script[type="application/ld+json"]').textContent());
      assert.equal(source['@type'], 'SoftwareSourceCode');
      assert.equal(source.codeRepository, 'https://github.com/bankerneel/wamt');
      assert.match(source.license, /\/LICENSE$/);
      assert.ok(source.name && source.description);
      const canonical = await page.locator('link[rel="canonical"]').getAttribute('href');
      const sitemap = await fetch(origin + '/sitemap.xml');
      assert.ok(sitemap.ok);
      assert.ok((await sitemap.text()).includes('<loc>' + canonical + '</loc>'));
      const socialImage = new URL(await page.locator('meta[property="og:image"]').getAttribute('content'));
      const image = await fetch(origin + socialImage.pathname);
      assert.equal(image.headers.get('content-type'), 'image/jpeg');
      await page.close();
      return 'three steps, three alibis, SoftwareSourceCode and canonical sitemap';
    });

    await check('theme toggle and aria-pressed', async () => {
      const page = await ctxDesktop.newPage();
      makeMonitors(page, 'theme');
      await goto(page, url);
      const read = () => page.evaluate(() => ({
        theme: document.documentElement.dataset.theme || getComputedStyle(document.documentElement).colorScheme || '',
        pressed: document.querySelector('[data-theme-toggle]')?.getAttribute('aria-pressed')
      }));
      const before = await read();
      await page.click('[data-theme-toggle]');
      await settle(page);
      const after = await read();
      assert.notEqual(after.theme, before.theme, 'theme did not change');
      assert.notEqual(after.pressed, before.pressed, 'aria-pressed did not change');
      await page.click('[data-theme-toggle]');
      await settle(page);
      const restored = await read();
      assert.equal(restored.theme, before.theme, 'theme did not toggle back');
      await page.close();
      return 'before=' + before.theme + ' after=' + after.theme + ' restored=' + restored.theme;
    });

    await check('copy button sets clipboard status', async () => {
      const page = await ctxDesktop.newPage();
      makeMonitors(page, 'copy');
      await goto(page, url);
      await scrollToSelector(page, '#install');
      const before = (await page.textContent('[data-copy-status]')) || '';
      await page.click('[data-copy]');
      await page.waitForFunction((prev) => {
        const el = document.querySelector('[data-copy-status]');
        return el && (el.textContent || '').trim() && el.textContent !== prev;
      }, before, { timeout: 5000 });
      const after = ((await page.textContent('[data-copy-status]')) || '').trim();
      let copied = '';
      try { copied = await page.evaluate(() => navigator.clipboard.readText()); } catch { /* permission may block */ }
      assert(after.length > 0, 'copy status stayed empty');
      if (copied) assert(copied.includes('cargo build'), 'clipboard did not contain quick start');
      const pose = page.locator('[data-scene="finale"] [data-sprite="coral"]');
      await page.waitForFunction(() => {
        const frame = Number(document.querySelector('[data-scene="finale"] [data-sprite="coral"]').dataset.frame);
        return frame >= 4 && frame <= 9;
      });
      await page.waitForFunction(() =>
        document.querySelector('[data-scene="finale"] [data-sprite="coral"]').dataset.frame === '11');
      assert.equal(await pose.getAttribute('data-frame'), '11');
      await page.close();
      return 'status="' + after + '", clipboardChecked=' + (copied.length > 0);
    });

    await check('keyboard menu opens and Escape restores focus', async () => {
      const ctx = await browser.newContext({ viewport: { width: 390, height: 844 }, isMobile: true, hasTouch: true });
      try {
        const page = await ctx.newPage();
        makeMonitors(page, 'menu');
        await goto(page, url);
        const toggle = await page.$('[data-menu-toggle]');
        assert(toggle, 'missing [data-menu-toggle]');
        await toggle.focus();
        await page.keyboard.press('Enter');
        await settle(page);
        const opened = await page.evaluate(() => {
          const t = document.querySelector('[data-menu-toggle]');
          return t && (t.getAttribute('aria-expanded') === 'true' || document.body.classList.contains('menu-open'));
        });
        assert(opened, 'menu did not open on Enter');
        await page.keyboard.press('Escape');
        await settle(page);
        const state = await page.evaluate(() => {
          const t = document.querySelector('[data-menu-toggle]');
          const closed = t && (t.getAttribute('aria-expanded') !== 'true' && !document.body.classList.contains('menu-open'));
          return { closed, focused: document.activeElement === t };
        });
        assert(state.closed, 'Escape did not close the menu');
        assert(state.focused, 'focus was not returned to the menu toggle');
      } finally {
        await ctx.close();
      }
      return 'Enter opens, Escape closes and restores focus';
    });

    await check('exhibit dialog opens and returns focus', async () => {
      const page = await ctxDesktop.newPage();
      makeMonitors(page, 'dialog');
      await goto(page, url);
      await scrollToSelector(page, '#evidence');
      const exhibit = await page.$('[data-exhibit]');
      assert(exhibit, 'missing [data-exhibit]');
      await exhibit.focus();
      await page.keyboard.press('Enter');
      await page.waitForFunction(() => {
        const d = document.querySelector('#evidence-dialog');
        return d && d.open;
      }, null, { timeout: 5000 });
      const close = await page.$('[data-exhibit-close]');
      assert(close, 'missing [data-exhibit-close]');
      await close.click();
      await settle(page);
      const state = await page.evaluate(() => {
        const d = document.querySelector('#evidence-dialog');
        const e = document.querySelector('[data-exhibit]');
        return { closed: d && !d.open, focused: document.activeElement === e };
      });
      assert(state.closed, 'dialog did not close');
      assert(state.focused, 'focus was not returned to the exhibit anchor');
      await page.close();
      return 'dialog opens, close returns focus';
    });

    await check('suspect reacts on hover or focus', async () => {
      const page = await ctxDesktop.newPage();
      makeMonitors(page, 'suspects');
      await goto(page, url);
      await scrollToSelector(page, '#suspects');
      const suspect = page.locator('#suspects [data-suspect]').first();
      const sprite = suspect.locator('[data-frame]');
      const before = await sprite.getAttribute('data-frame');
      await suspect.focus();
      await settle(page);
      const after = await sprite.getAttribute('data-frame');
      assert.notEqual(after, before, 'suspect data-frame did not change (before ' + before + ')');
      await page.close();
      return 'frame ' + before + ' -> ' + after;
    });

    await check('cookie frames reverse on scroll back and history back', async () => {
      const page = await ctxDesktop.newPage();
      makeMonitors(page, 'cookie');
      await goto(page, url);
      const read = () => page.evaluate(() => {
        const el = document.querySelector('.eating-stage [data-sprite="cookie"]') || document.querySelector('.sprite[data-sprite="cookie"][data-frame]');
        return el ? el.dataset.frame : null;
      });
      await scrollY(page, 0);
      const atHero = await read();
      const midY = await findMidScroll(page);
      await scrollY(page, midY);
      const atMid = await read();
      const canvas = page.locator('[data-scene="stage"] canvas[data-crumbs-painted]');
      assert.ok(await canvas.evaluate(el => el.getContext('2d').getImageData(0, 0, el.width, el.height)
        .data.some((alpha, i) => i % 4 === 3 && alpha > 0)), 'direct scroll to a bite should paint crumbs');
      await scrollToSelector(page, '#install');
      const atInstall = await read();
      await scrollY(page, midY);
      const backMid = await read();
      await scrollY(page, 0);
      const backHero = await read();
      assert(atHero != null && atMid != null && atInstall != null, 'cookie data-frame not readable');
      assert.equal(backMid, atMid, 'cookie frame at mid changed after scrolling away and back');
      assert.equal(backHero, atHero, 'cookie frame at hero changed after scrolling away and back');
      assert(Number(atInstall) >= Number(atMid), 'cookie did not advance toward install');
      for (const state of [1, 2, 3, 4, 5, 6, 7, 6, 5, 4, 3, 2, 1, 0]) {
        const position = state ? await page.locator('[data-beat="' + state + '"]').evaluate(el =>
          scrollY + el.getBoundingClientRect().top - innerHeight * 0.25) : 0;
        await scrollY(page, position);
        assert.equal(Number(await read()), state);
        assert.equal(await page.locator('[data-favicon]').getAttribute('href'), '/img/sprites/favicon-' + state + '.png');
        assert.equal(await page.locator('[data-plate-meter]').evaluate(meter => meter.value), 7 - state);
        if (state === 7) {
          assert.equal(await page.locator('[data-plate-label]').textContent(), 'plate empty');
          assert.equal(await page.locator('#finale-plate').evaluate(meter => meter.value), 0);
        }
      }
      let historyNote = 'scroll reversal ok';
      if ((await page.evaluate(() => history.length)) > 1) {
        await page.goBack().catch(() => {});
        await settle(page);
        historyNote += ', historyBack=' + (await read());
      }
      await page.close();
      return 'hero=' + atHero + ' mid=' + atMid + ' install=' + atInstall + ' backMid=' + backMid + ' backHero=' + backHero + '; ' + historyNote;
    });

    await check('favicon and hidden-document title', async () => {
      const page = await ctxDesktop.newPage();
      makeMonitors(page, 'favicon');
      await goto(page, url);
      const icon = await page.evaluate(() => {
        const l = document.querySelector('link[rel~="icon"]');
        return l ? l.getAttribute('href') : null;
      });
      const title = await page.title();
      assert(icon, 'no favicon link');
      assert(/\.(svg|png|ico)(\?|$)/i.test(icon), 'favicon is not an image: ' + icon);
      assert(title && title.length > 0, 'empty document title');
      const backgroundTitle = await page.evaluate(() => {
        Object.defineProperty(document, 'hidden', { value: true, configurable: true });
        document.dispatchEvent(new Event('visibilitychange'));
        const title = document.title;
        delete document.hidden;
        document.dispatchEvent(new Event('visibilitychange'));
        return { title, restored: document.title };
      });
      assert.equal(backgroundTitle.title, 'there is still cookie left');
      assert.equal(backgroundTitle.restored, title);
      await page.close();
      return 'favicon=' + icon + ', title swaps and restores on a simulated visibility change';
    });

    await check('camera and horizontal bounds in both themes', async () => {
      const page = await ctxDesktop.newPage();
      makeMonitors(page, 'overscroll');
      const report = [];
      for (const colorScheme of THEMES) {
        await page.emulateMedia({ colorScheme });
        for (const width of [320, 390, 768, 1024, 1440]) {
          await page.setViewportSize({ width, height: 844 });
          await goto(page, url);
          await page.waitForFunction(() => document.querySelector('[data-scene="hero"]').dataset.pose === 'peek');
          await checkHeroPixels(page);
          await scrollY(page, 88);
          await checkHeroPixels(page);
          const overflow = await page.evaluate(() => Math.max(
            document.documentElement.scrollWidth - document.documentElement.clientWidth,
            document.body.scrollWidth - document.body.clientWidth
          ));
          assert.equal(overflow, 0, colorScheme + ' at ' + width + 'px');
          report.push(colorScheme + '/' + width);
        }
      }
      await page.close();
      return report.join(', ');
    });

    await check('every evidence caption clears every other card', async () => {
      const page = await ctxDesktop.newPage();
      makeMonitors(page, 'captions');
      for (const width of [390, 768, 1024, 1440]) {
        await page.setViewportSize({ width, height: 844 });
        await goto(page, url);
        await scrollToSelector(page, '#evidence');
        const coverage = await page.locator('#evidence figure').evaluateAll(cards => {
          const hit = (a, b) => a.left < b.right && a.right > b.left && a.top < b.bottom && a.bottom > b.top;
          const rects = cards.map(card => card.getBoundingClientRect());
          return cards.map((card, i) => {
            const caption = card.querySelector('figcaption').getBoundingClientRect();
            return rects.some((rect, j) => j !== i && hit(caption, rect));
          });
        });
        assert.deepEqual(coverage, Array(7).fill(false), 'caption overlap at ' + width + 'px');
      }
      await page.close();
      return 'all 7 clear at 390, 768, 1024 and 1440';
    });

    await check('no console warnings, errors or page errors', async () => {
      assert.equal(consoleErrors.length, 0, 'console errors: ' + consoleErrors.slice(0, 5).join(' | '));
      assert.equal(consoleWarnings.length, 0, 'console warnings: ' + consoleWarnings.slice(0, 5).join(' | '));
      assert.equal(pageErrors.length, 0, 'page errors: ' + pageErrors.slice(0, 5).join(' | '));
      return 'errors=0, pageErrors=0, warnings=' + consoleWarnings.length;
    });

    await check('no non-origin requests', async () => {
      assert.equal(violations.length, 0, 'non-origin requests: ' + JSON.stringify(violations.slice(0, 5)));
      return 'all captured requests stayed on ' + origin;
    });

    await check('JavaScript off: readable h1, links and quick start', async () => {
      const ctx = await browser.newContext({ viewport: { width: 1440, height: 900 }, javaScriptEnabled: false });
      try {
        const page = await ctx.newPage();
        makeMonitors(page, 'js-off');
        // page.evaluate never resolves with scripting disabled, so inspect the
        // DOM through the CDP DOM domain instead of the page's own JavaScript.
        await page.goto(url, { waitUntil: 'load', timeout: 30000 });
        const cdp = await ctx.newCDPSession(page);
        await cdp.send('DOM.enable');
        const doc = await cdp.send('DOM.getDocument', { depth: -1 });
        async function nodes(selector) {
          const r = await cdp.send('DOM.querySelectorAll', { nodeId: doc.root.nodeId, selector });
          return r.nodeIds || [];
        }
        async function plainText(selector, index) {
          const ids = await nodes(selector);
          const id = ids[index || 0];
          if (!id) return '';
          const html = await cdp.send('DOM.getOuterHTML', { nodeId: id });
          return html.outerHTML.replace(/<[^>]*>/g, ' ').replace(/\s+/g, ' ').trim();
        }
        const codeIds = await nodes('pre, code');
        let code = '';
        for (const id of codeIds) {
          const html = await cdp.send('DOM.getOuterHTML', { nodeId: id });
          code += html.outerHTML.replace(/<[^>]*>/g, ' ').replace(/\s+/g, ' ') + '\n';
        }
        const info = {
          h1: (await nodes('h1')).length,
          h1Text: await plainText('h1', 0),
          links: (await nodes('a[href]')).length,
          text: (await plainText('body', 0)).length,
          code
        };
        assert.equal(info.h1, 1, 'expected exactly one h1, got ' + info.h1);
        assert(info.h1Text.length > 0, 'h1 has no text');
        assert(info.links >= 5, 'only ' + info.links + ' links found');
        assert(info.text > 300, 'body text too short: ' + info.text);
        assert(info.code.includes('cargo build --release'), 'quick start missing cargo build');
        assert(info.code.includes('scripts/wamt start'), 'quick start missing start');
        assert(info.code.includes('scripts/wamt open'), 'quick start missing open');
        assert.equal((await nodes('#case-notes li')).length, 3);
        assert.equal((await nodes('#suspects .suspect-alibi')).length, 3);
        assert.match(await plainText('label[for="finale-plate"]'), /Plate empty/);
        return 'h1="' + info.h1Text.slice(0, 40) + '", links=' + info.links + ', text=' + info.text;
      } finally {
        await ctx.close();
      }
    });

    await check('reduced-motion state and final still', async () => {
      const ctx = await browser.newContext({ viewport: { width: 1440, height: 900 }, reducedMotion: 'reduce' });
      try {
        const page = await ctx.newPage();
        makeMonitors(page, 'reduced-motion');
        await goto(page, url);
        const info = await page.evaluate(() => {
          const s = document.querySelector('.scene') || document.querySelector('body');
          const cs = getComputedStyle(s);
          return {
            matches: matchMedia('(prefers-reduced-motion: reduce)').matches,
            animName: cs.animationName,
            animDuration: cs.animationDuration,
            transDuration: cs.transitionDuration
          };
        });
        assert(info.matches, 'reduce media query not active');
        const noAnim = info.animName === 'none' || parseFloat(info.animDuration) === 0 || info.animDuration === '0s';
        assert(noAnim, 'animation still active under reduce: ' + JSON.stringify(info));
        assert.equal(await page.locator(HERO).getAttribute('data-pose'), 'still');
        assert.deepEqual(await heroFrames(page), [9, 9, 9]);
        await scrollToSelector(page, '#install');
        const finalFrame = await page.evaluate(() => {
          const el = document.querySelector('.eating-stage [data-sprite="cookie"]') || document.querySelector('.sprite[data-sprite="cookie"][data-frame]');
          return el ? el.dataset.frame : null;
        });
        if (finalFrame != null) assert.equal(Number(finalFrame), 7, 'cookie is not at final crumb frame under reduce');
        return 'reduce active, animations off, finalFrame=' + finalFrame;
      } finally {
        await ctx.close();
      }
    });

    // Lighthouse mobile pass.
    log('functional checks done, lighthouse phase');
    if (NO_LH) {
      results.push({ name: 'lighthouse mobile scores', status: 'SKIP', detail: 'skipped via --no-lighthouse', ms: 0 });
    } else {
      await check('lighthouse mobile scores', async () => {
        const lhPath = tooling.req.resolve('lighthouse');
        const clPath = tooling.req.resolve('chrome-launcher');
        const lighthouse = (await import(pathToFileURL(lhPath).href)).default;
        const chromeLauncher = await import(pathToFileURL(clPath).href);
        const chrome = await chromeLauncher.launch({
          chromePath: tooling.chromiumExe,
          chromeFlags: ['--headless=new', '--disable-gpu', '--disable-background-networking', '--disable-sync'],
          logLevel: 'error'
        });
        try {
          const raw = await lighthouse(url, {
            port: chrome.port,
            output: 'json',
            logLevel: 'error',
            onlyCategories: ['performance', 'accessibility', 'best-practices', 'seo'],
            disableStorageReset: true
          }, {
            extends: 'lighthouse:default',
            settings: {
              formFactor: 'mobile',
              throttlingMethod: 'simulate',
              screenEmulation: { mobile: true, width: 360, height: 640, deviceScaleFactor: 1, disabled: false }
            }
          });
          const r = raw.lhr || raw;
          if (LH_JSON) {
            fs.mkdirSync(path.dirname(path.resolve(LH_JSON)), { recursive: true });
            fs.writeFileSync(LH_JSON, JSON.stringify(r, null, 2));
          }
          const pct = (c) => Math.round(((r.categories[c] && r.categories[c].score) || 0) * 100);
          const lcpAudit = r.audits['largest-contentful-paint'] || {};
          const clsAudit = r.audits['cumulative-layout-shift'] || {};
          lighthouseResult = {
            performance: pct('performance'),
            accessibility: pct('accessibility'),
            bestPractices: pct('best-practices'),
            seo: pct('seo'),
            lcp: lcpAudit.displayValue || 'n/a',
            cls: clsAudit.displayValue || 'n/a',
            lcpMs: lcpAudit.numericValue,
            clsValue: clsAudit.numericValue
          };
          assert(lighthouseResult.performance >= 95, 'performance below 95');
          assert(lighthouseResult.accessibility >= 95, 'accessibility below 95');
          assert(lighthouseResult.bestPractices >= 95, 'best practices below 95');
          assert(lighthouseResult.seo >= 100, 'SEO below 100');
          if (Number.isFinite(lighthouseResult.lcpMs)) assert(lighthouseResult.lcpMs < 2000, 'LCP over 2s');
          if (Number.isFinite(lighthouseResult.clsValue)) assert(lighthouseResult.clsValue < 0.1, 'CLS over 0.1');
          return 'perf=' + lighthouseResult.performance + ' a11y=' + lighthouseResult.accessibility +
            ' bp=' + lighthouseResult.bestPractices + ' seo=' + lighthouseResult.seo +
            ' LCP=' + lighthouseResult.lcp + ' CLS=' + lighthouseResult.cls;
        } finally {
          const killed = chrome.kill();
          if (killed && typeof killed.catch === 'function') await killed.catch(() => {});
        }
      });
    }

    await ctxDesktop.close();
    log('desktop context closed, building summary');

    const lines = [];
    lines.push('Acceptance harness summary');
    lines.push('site   : ' + SITE_ROOT);
    lines.push('out    : ' + OUT_DIR + ' (' + screenshots.length + ' screenshots)');
    lines.push('origin : ' + origin);
    if (transfer) lines.push('weight : ' + JSON.stringify(transfer));
    if (lighthouseResult) lines.push('lighthouse: ' + JSON.stringify(lighthouseResult));
    if (consoleWarnings.length) lines.push('console warnings (' + consoleWarnings.length + '): ' + consoleWarnings.slice(0, 8).join(' | '));
    if (notes.length) lines.push('notes: ' + notes.slice(0, 8).join(' | '));
    lines.push('');
    for (const r of results) {
      lines.push(r.status.padEnd(4) + '  ' + r.name + (r.detail ? '  :: ' + r.detail : ''));
    }
    const failed = results.filter((r) => r.status === 'FAIL').length;
    const skipped = results.filter((r) => r.status === 'SKIP').length;
    lines.push('');
    lines.push('PASS=' + results.filter((r) => r.status === 'PASS').length + ' FAIL=' + failed + ' SKIP=' + skipped);
    console.log('\n' + lines.join('\n'));

    if (failed) process.exitCode = 1;
  } catch (e) {
    console.error('[check] fatal:', (e && e.stack) || e);
    process.exitCode = 1;
  } finally {
    log('closing browser and server');
    if (browser) await withTimeout(browser.close(), 30000, 'browser.close').catch(() => {});
    await withTimeout(new Promise((resolve) => server.close(resolve)), 15000, 'server.close').catch(() => {});
    if (typeof server.closeAllConnections === 'function') server.closeAllConnections();
    log('server stop', stamp(), origin);
  }
}

main();
