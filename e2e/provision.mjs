// An approved member creates a mint from the page; it lands as a commit (run by e2e/run.sh).
import assert from 'node:assert/strict';
import {execFileSync} from 'node:child_process';
import {schnorr} from '@noble/curves/secp256k1.js';
import {sha256} from '@noble/hashes/sha2.js';
import {chromium} from 'playwright';

const [base, shots, bin, dataDir, remote] = process.argv.slice(2);
const hex = (b) => Buffer.from(b).toString('hex');
const secret = new Uint8Array(32).fill(11);
const pub = schnorr.getPublicKey(secret);
const npub = hex(pub);
const browser = await chromium.launch();
const problems = [];
const ctx = await browser.newContext({viewport: {width: 1280, height: 900}});
await ctx.exposeFunction('__sign', (ev) => {
  const e = {...ev, pubkey: hex(pub)};
  const id = sha256(new TextEncoder().encode(JSON.stringify([0, e.pubkey, e.created_at, e.kind, e.tags, e.content])));
  return {...e, id: hex(id), sig: hex(schnorr.sign(id, secret))};
});
await ctx.addInitScript(() => {
  window.nostr = {getPublicKey: async () => '', signEvent: (ev) => window.__sign(ev)};
});
const page = await ctx.newPage();
page.on('console', (m) => {
  if (m.type() === 'error') problems.push(m.text());
});
page.on('pageerror', (e) => problems.push(e.message));

await page.goto(base);
await page.click('[data-sign]');
await page.waitForSelector('.beta__result--ok');
assert.ok(await page.locator('[data-mint-panel]').isHidden(), 'no panel before approval');
console.log('signed up, not approved: no mint panel');

const env = {...process.env, DATA_DIR: dataDir, GITOPS_REPO: remote};
console.log(execFileSync(bin, ['approve', npub], {env}).toString().trim());

await page.reload();
await page.click('[data-sign]');
await page.waitForSelector('[data-mint-panel]:not([hidden])');
await page.fill('input[name=mint]', 'Sat_oshi!');
assert.equal(await page.inputValue('input[name=mint]'), 'satoshi', 'name is cleaned as typed');
assert.equal(await page.textContent('[data-mint-preview]'), 'satoshi');
await page.click('[data-create-mint]');
await page.waitForSelector('.mint-card');
const card = await page.textContent('.mint-card');
assert.match(card, /satoshi\.run\.lnurlcash\.com/);
assert.match(card, /mint@satoshi\.run\.lnurlcash\.com/);
assert.match(card, /Committed in [0-9a-f]{7}/);
console.log('created:', card.replace(/\s+/g, ' ').trim());
await page.locator('#beta').screenshot({path: shots + '/beta-mint-created.png'});

// signing in again shows it, and no second one can be made
await page.reload();
await page.click('[data-sign]');
await page.waitForSelector('.mint-card');
assert.ok(await page.locator('.mint-panel__row').isHidden());
console.log('signed in again: the mint is listed');

const log = execFileSync('git', ['log', '--format=%an|%s', 'main'], {cwd: remote}).toString().trim();
assert.match(log, /^lnurlcash-run\|run: provision satoshi for npub1/);
console.log('remote:', log);
await browser.close();
assert.deepEqual(problems, [], 'console errors');
console.log('PASS');
