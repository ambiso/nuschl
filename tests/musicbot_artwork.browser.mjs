// SPDX-License-Identifier: EUPL-1.2
// Actual packaged musicbot UI under its production CSP; owned HTTP services only.
import assert from 'node:assert/strict';
import { writeFile } from 'node:fs/promises';
import { pathToFileURL } from 'node:url';
const [link, executablePath, playwright, output, proxyPort] = process.argv.slice(2);
const { chromium } = await import(pathToFileURL(playwright));
const browser = await chromium.launch({ executablePath, headless: true, args: [
  `--proxy-server=http://127.0.0.1:${proxyPort}`, '--proxy-bypass-list=127.0.0.1;localhost', '--dns-prefetch-disable',
] });
const origin = new URL(link).origin;
const proof = { viewports: [], stale: null, malformed: null };
async function setup(viewport) {
  const context = await browser.newContext({ viewport });
  const page = await context.newPage();
  const failures = [];
  await page.addInitScript(() => {
    window.fixtureViolations = []; window.fixtureMediaRequests = 0;
    document.addEventListener('securitypolicyviolation', e => window.fixtureViolations.push(e.violatedDirective));
    if (navigator.mediaDevices) navigator.mediaDevices.getUserMedia = () => { window.fixtureMediaRequests++; throw Error('fixture forbids capture'); };
  });
  page.on('request', request => { if (new URL(request.url()).origin !== origin) failures.push(request.url()); });
  page.on('pageerror', error => failures.push(String(error)));
  return { context, page, failures };
}
async function pixel(page, expected) {
  // Covers are same-origin PNGs, so a canvas may read them back.
  await page.waitForFunction(color => [...document.querySelectorAll('img.ready')].some(img => {
    const canvas = document.createElement('canvas');
    canvas.width = canvas.height = 1;
    const context = canvas.getContext('2d');
    context.drawImage(img, 0, 0, 1, 1);
    const p = context.getImageData(0, 0, 1, 1).data;
    return color.every((v, i) => Math.abs(v - p[i]) <= 2);
  }), expected);
}
async function choose(page, library) {
  const menu = page.getByRole('button', { name: 'Open navigation', exact: true });
  if (await menu.isVisible()) await menu.click();
  await page.getByLabel('Library', { exact: true }).selectOption(String(library));
  const close = page.getByRole('button', { name: 'Close navigation', exact: true });
  if (await close.isVisible()) await close.click();
}
async function clean(page, failures, violations = []) {
  assert.deepEqual(failures, []);
  assert.deepEqual(await page.evaluate(() => window.fixtureViolations), violations);
  assert.equal(await page.evaluate(() => window.fixtureMediaRequests), 0);
}
try {
  for (const width of [320, 390, 1280]) {
    const { context, page, failures } = await setup({ width, height: 800 });
    const requested = new Set();
    page.on('request', request => { if (new URL(request.url()).pathname.startsWith('/art/')) requested.add(request.url()); });
    await page.goto(link + '#/albums?library=0&list=newest');
    await pixel(page, [200, 80, 40, 255]);
    await page.waitForFunction(() => {
      const viewport = document.querySelector('#main-content').getBoundingClientRect();
      return [...document.querySelectorAll('.media-grid img')].every(img => {
        const r = img.getBoundingClientRect();
        const visible = r.bottom > viewport.top && r.top < viewport.bottom && r.right > viewport.left && r.left < viewport.right;
        return !visible || img.classList.contains('ready');
      });
    });
    const before = await page.evaluate(() => ({
      total: document.querySelectorAll('.media-grid img').length,
      shown: document.querySelectorAll('.media-grid img.ready').length,
      overflow: document.documentElement.scrollWidth > innerWidth,
    }));
    assert.equal(before.total, 48); assert.equal(before.overflow, false);
    // Covers far below the fold are not fetched until they come near.
    assert.ok(before.shown > 0 && requested.size < 48, { ...before, requested: requested.size });
    await page.screenshot({ path: `${output}/covers-${width}.png` });
    await choose(page, 1);
    await pixel(page, [40, 80, 200, 255]);
    await clean(page, failures);
    proof.viewports.push({ width, ...before, requestedBeforeScrolling: requested.size, libraryPixelsChanged: true });
    await context.close();
  }
  {
    const { context, page, failures } = await setup({ width: 1280, height: 800 });
    let release, entered;
    const enteredPromise = new Promise(r => entered=r);
    const held = new Promise(r => release=r);
    let first = true, discarded = false;
    await page.route('**/art/0/**', async route => {
      if (!first) return route.continue();
      first = false;
      const response = await route.fetch();
      entered(); await held;
      try { await route.fulfill({ response }); } catch { discarded = true; }
    });
    await page.goto(link + '#/albums?library=0&list=newest');
    await enteredPromise;
    await choose(page, 1);
    await pixel(page, [40, 80, 200, 255]);
    release();
    await page.waitForTimeout(150);
    assert.equal(await page.evaluate(() => [...document.querySelectorAll('img.ready')].some(img => img.src.includes('/art/0/'))), false);
    await clean(page, failures);
    proof.stale = { heldRealResponse: true, noOldCoverAfterNavigation: true, abortedFulfillRejected: discarded };
    await context.close();
  }
  {
    const { context, page, failures } = await setup({ width: 390, height: 800 });
    // An answer that is no image keeps the placeholder.
    await page.route('**/art/**', route => route.fulfill({ status:200,contentType:'application/json',body:JSON.stringify({ width:1,height:1,rgba:'yFAo/w==' }) }));
    await page.goto(link + '#/albums?library=0&list=newest');
    await page.waitForSelector('.media-grid img');
    await page.waitForTimeout(250);
    assert.equal(await page.locator('img.ready').count(), 0);
    assert.ok(await page.locator('.media-grid .placeholder').count() > 0);
    // Images from anywhere else are refused by the CSP before any request.
    await page.evaluate(() => { const img = document.createElement('img'); img.src = 'https://never-fetch.invalid/cover.png'; document.body.append(img); });
    await page.waitForFunction(() => window.fixtureViolations.length > 0);
    await clean(page, failures, ['img-src']);
    proof.malformed = { notAnImageKeptPlaceholder: true, foreignImageRefusedByCsp: true, remoteFetches: 0 };
    await context.close();
  }
  await writeFile(`${output}/browser-proof.json`, JSON.stringify(proof,null,2)+'\n');
  console.log(JSON.stringify(proof));
} finally { await browser.close(); }
