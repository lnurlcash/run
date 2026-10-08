// The landing page and its beta sign-up, at desktop and phone width (run by e2e/run.sh).
import assert from 'node:assert/strict';
import {schnorr} from '@noble/curves/secp256k1.js';
import {sha256} from '@noble/hashes/sha2.js';
import {chromium} from 'playwright';

const base = process.argv[2],
  shots = process.argv[3];
const hex = (b) => Buffer.from(b).toString('hex');
const secret = new Uint8Array(32).fill(9);
const pubkey = hex(schnorr.getPublicKey(secret));

const browser = await chromium.launch();
const problems = [];
const watch = (page) => {
  page.on('console', (m) => {
    if (m.type() === 'error') problems.push(m.text());
  });
  page.on('pageerror', (e) => problems.push(e.message));
};

// desktop, with a NIP-07 signer
const ctx = await browser.newContext({viewport: {width: 1440, height: 900}});
await ctx.exposeFunction('__sign', (ev) => {
  const e = {...ev, pubkey};
  const id = sha256(new TextEncoder().encode(JSON.stringify([0, e.pubkey, e.created_at, e.kind, e.tags, e.content])));
  return {...e, id: hex(id), sig: hex(schnorr.sign(id, secret))};
});
await ctx.addInitScript(() => {
  window.nostr = {getPublicKey: async () => '', signEvent: (ev) => window.__sign(ev)};
});
const page = await ctx.newPage();
watch(page);
await page.goto(base);
await page.screenshot({path: shots + '/desktop-hero.png'});
// scroll through so every reveal fires, then a full-page shot
for (let y = 0; y < 12000; y += 600) {
  await page.mouse.wheel(0, 600);
  await page.waitForTimeout(60);
}
await page.waitForTimeout(900);
await page.screenshot({path: shots + '/desktop-full.png', fullPage: true});

await page.fill('textarea[name=note]', 'A mint for our meetup — "quotes" & ⚡');
await page.click('[data-sign]');
await page.waitForSelector('.beta__result--ok');
const text = await page.textContent('[data-result]');
assert.match(text, /#1 on the list/);
assert.ok(!/unverified/.test(text), text);
console.log('signed sign-up:', text.trim());
await page.locator('#beta').screenshot({path: shots + '/desktop-beta-joined.png'});
await page.waitForSelector('[data-count]:not([hidden])');

// phone, no extension: falls back to a typed npub
const phone = await browser.newContext({viewport: {width: 390, height: 844}, isMobile: true, hasTouch: true});
const p2 = await phone.newPage();
watch(p2);
await p2.goto(base);
await p2.screenshot({path: shots + '/phone-hero.png'});
const width = await p2.evaluate(() => document.documentElement.scrollWidth);
assert.ok(width <= 390, 'horizontal scroll on a phone: ' + width);
await p2.click('[data-sign]');
await p2.waitForSelector('[data-no-extension]:not([hidden])');
await p2.fill('input[name=npub]', 'npub10elfcs4fr0l0r8af98jlmgdh9c8tcxjvz9qkw038js35mp4dma8qzvjptg');
await p2.click('[data-typed]');
await p2.waitForSelector('.beta__result--ok');
const t2 = await p2.textContent('[data-result]');
assert.match(t2, /#2 on the list/);
assert.match(t2, /unverified/);
console.log('typed sign-up:', t2.trim());
await p2.screenshot({path: shots + '/phone-full.png', fullPage: true});
await p2.fill('input[name=npub]', 'npub1nope');
await p2.click('[data-typed]');
await p2.waitForSelector('.beta__result--error');
console.log('bad npub:', (await p2.textContent('[data-result]')).trim());

await browser.close();
// a 400 for the bad npub is expected
const unexpected = problems.filter((p) => !p.includes('400'));
assert.deepEqual(unexpected, [], 'console errors');
console.log('PASS');
