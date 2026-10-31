const {test} = require('node:test');
const assert = require('node:assert/strict');
const path = require('node:path');
const {pathToFileURL} = require('node:url');

const modules = name => import(pathToFileURL(path.join(__dirname, '../static', name)).href);
const define = (name, value) => Object.defineProperty(globalThis, name, {value, configurable: true, writable: true});

async function browser(responses) {
  const {store} = await modules('lib/core.js');
  const {submitSetup} = await modules('pages/auth.js');
  const calls = [], redirects = [], storage = new Map(), main = {innerHTML: ''}, timers = [];
  define('document', {querySelector: () => main});
  define('location', {pathname: '/setup', hash: '', assign: url => redirects.push(url)});
  define('sessionStorage', {setItem: (key, value) => storage.set(key, value)});
  define('fetch', async (url, options) => {
    calls.push({url, options});
    assert.ok(responses.length, 'unexpected request');
    const {status, body} = responses.shift();
    return {ok: status < 400, status, json: async () => body};
  });
  store.session = {csrf: 'original'};
  return {store, calls, redirects, storage, main, timers,
    async submit() {
      // Capture the restart delay without disturbing the test runner's own timers.
      const original = globalThis.setTimeout;
      globalThis.setTimeout = (callback, delay) => timers.push({callback, delay});
      try { await submitSetup({username: 'owner', password: 'a-test-password', token: 'test-token'}); }
      finally { globalThis.setTimeout = original; }
    }};
}

test('an unsuccessful response after owner creation leads to sign-in without resubmitting setup', async () => {
  const app = await browser([{status: 409, body: {error: 'Already set up'}}, {status: 200, body: {setup: false, csrf: 'fresh'}}]);
  await app.submit();
  assert.deepEqual(app.calls.map(c => c.url), ['/api/setup', '/api/session']);
  assert.deepEqual(app.redirects, ['/login']);
  assert.match(app.storage.get('loki-setup-notice'), /already set up/);
});

test('a rejected token keeps setup open and refreshes the browser session for retry', async () => {
  const app = await browser([{status: 403, body: {error: 'The setup token is invalid'}}, {status: 200, body: {setup: true, csrf: 'fresh'}}]);
  await assert.rejects(app.submit(), /setup token is invalid/);
  assert.deepEqual(app.redirects, []);
  assert.equal(app.store.session.csrf, 'fresh');
});

test('a relocation warning is carried to sign-in after account creation', async () => {
  const warning = 'Your owner account was created, but the data directory could not be moved.';
  const app = await browser([{status: 200, body: {ok: true, warning}}]);
  await app.submit();
  assert.deepEqual(app.redirects, ['/login']);
  assert.equal(app.storage.get('loki-setup-notice'), warning);
});

test('successful relocation waits for restart before navigating to sign-in', async () => {
  const app = await browser([{status: 200, body: {ok: true, restarting: true}}]);
  await app.submit();
  assert.deepEqual(app.redirects, []);
  assert.match(app.main.innerHTML, /restarting/);
  assert.equal(app.timers[0].delay, 3000);
  app.timers[0].callback();
  assert.deepEqual(app.redirects, ['/login']);
});
