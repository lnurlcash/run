// An approved member creates a mint from the page, closes it and deletes it once
// drained; each lands as a commit (run by e2e/run.sh).
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
await page.waitForSelector('.mint-panel__out .bad');
assert.match(await page.textContent('.mint-panel__out'), /sweep|address/i, 'a sweep address is asked for');
await page.fill('input[name=sweep]', 'tb1qw508d6qejxtdg4y5r3zarvary0c5xw7kxpjzsx');
await page.click('[data-create-mint]');
await page.waitForSelector('.mint-panel__out .bad:has-text("another network")');
const sweep = 'bc1qw508d6qejxtdg4y5r3zarvary0c5xw7kv8f3t4';
await page.fill('input[name=sweep]', sweep);
await page.click('[data-create-mint]');
await page.waitForSelector('.mint-card');
const card = await page.textContent('.mint-card');
assert.match(card, /satoshi\.run\.lnurlcash\.com/);
assert.match(card, /mint@satoshi\.run\.lnurlcash\.com/);
assert.match(card, /Committed in [0-9a-f]{7}/);
assert.ok(card.includes(`Sweeps to ${sweep}`), card);
console.log('created:', card.replace(/\s+/g, ' ').trim());
await page.locator('#beta').screenshot({path: shots + '/beta-mint-created.png'});

// signing in again shows it, and no second one can be made
await page.reload();
await page.click('[data-sign]');
await page.waitForSelector('.mint-card');
assert.ok(await page.locator('[data-mint-new]').isHidden());
console.log('signed in again: the mint is listed');

// closing: the mint's own /closing report, first with a channel left, then drained
let drained = false;
await page.route('https://satoshi.run.lnurlcash.com/closing', (route) =>
  route.fulfill({
    contentType: 'application/json',
    headers: {'Access-Control-Allow-Origin': '*'},
    body: JSON.stringify({
      closing: true,
      sweep_address: sweep,
      channels: drained ? 0 : 1,
      lightning_sat: drained ? 0 : 52000,
      sweeping_sat: 0,
      onchain_sat: 0,
      drained,
    }),
  }),
);
await page.click('.mint-card button:has-text("Close mint")');
await page.click('.mint-card button:has-text("Close every channel")');
await page.waitForSelector('.mint-card :text("1 channel open")');
assert.ok(await page.locator('.mint-card button:has-text("Delete mint")').isDisabled(), 'no delete before drained');
let spec = execFileSync('git', ['show', 'main:mints/satoshi/resources.yaml'], {cwd: remote}).toString();
assert.match(spec, /"CLOSE_ALL"/);
assert.ok(spec.includes(sweep));
console.log('closing: CLOSE_ALL committed, delete waits');
await page.locator('#beta').screenshot({path: shots + '/beta-mint-closing.png'});

drained = true;
await page.reload();
await page.click('[data-sign]');
await page.waitForSelector('.mint-card :text("You can delete the mint now")');
await page.click('.mint-card button:has-text("Delete mint")');
await page.click('.mint-card button:has-text("Delete it for good")');
await page.waitForSelector(':text("satoshi is deleted.")');
spec = execFileSync('git', ['ls-tree', '-r', '--name-only', 'main'], {cwd: remote}).toString();
assert.ok(!spec.includes('mints/satoshi/'), spec);
console.log('drained: deleted');

const log = execFileSync('git', ['log', '--format=%an|%s', 'main'], {cwd: remote}).toString().trim();
assert.match(
  log,
  /^lnurlcash-run\|run: delete satoshi\nlnurlcash-run\|run: close satoshi\nlnurlcash-run\|run: provision satoshi for npub1/,
);
console.log('remote:', log.replace(/\n/g, ' / '));
await browser.close();
// the one expected: the create refused for another network's address
const refused = problems.indexOf('Failed to load resource: the server responded with a status of 400 (Bad Request)');
assert.ok(refused >= 0, 'the wrong-network create was refused');
problems.splice(refused, 1);
assert.deepEqual(problems, [], 'console errors');
console.log('PASS');
