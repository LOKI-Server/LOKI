const {test} = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const path = require('node:path');

function browser(responses) {
  const calls = [], redirects = [], storage = new Map(), main = {innerHTML: ''}, timers = [];
  const context = vm.createContext({
    FormData,
    document: {querySelector: () => main},
    location: {pathname: '/setup', assign: url => redirects.push(url)},
    sessionStorage: {setItem: (key, value) => storage.set(key, value)},
    setTimeout: (callback, delay) => timers.push({callback, delay}),
    fetch: async (url, options) => {
      calls.push({url, options});
      assert.ok(responses.length, 'unexpected request');
      const {status, body} = responses.shift();
      return {ok: status < 400, status, json: async () => body};
    }
  });
  const script = fs.readFileSync(path.join(__dirname, '../static/app.js'), 'utf8');
  vm.runInContext(script.slice(0, script.indexOf('function nav()')), context);
  vm.runInContext('session={csrf:"original"}', context);
  return {context, calls, redirects, storage, main, timers,
    submit: () => vm.runInContext('submitSetup({username:"owner",password:"a-test-password",token:"test-token"})', context)};
}

test('an unsuccessful response after owner creation leads to sign-in without resubmitting setup', async () => {
  const app = browser([{status: 409, body: {error: 'Already set up'}}, {status: 200, body: {setup: false, csrf: 'fresh'}}]);
  await app.submit();
  assert.deepEqual(app.calls.map(c => c.url), ['/api/setup', '/api/session']);
  assert.deepEqual(app.redirects, ['/login']);
  assert.match(app.storage.get('loki-setup-notice'), /already set up/);
});

test('a rejected token keeps setup open and refreshes the browser session for retry', async () => {
  const app = browser([{status: 403, body: {error: 'The setup token is invalid'}}, {status: 200, body: {setup: true, csrf: 'fresh'}}]);
  await assert.rejects(app.submit(), /setup token is invalid/);
  assert.deepEqual(app.redirects, []);
  assert.equal(vm.runInContext('session.csrf', app.context), 'fresh');
});

test('a relocation warning is carried to sign-in after account creation', async () => {
  const warning = 'Your owner account was created, but the data directory could not be moved.';
  const app = browser([{status: 200, body: {ok: true, warning}}]);
  await app.submit();
  assert.deepEqual(app.redirects, ['/login']);
  assert.equal(app.storage.get('loki-setup-notice'), warning);
});

test('successful relocation waits for restart before navigating to sign-in', async () => {
  const app = browser([{status: 200, body: {ok: true, restarting: true}}]);
  await app.submit();
  assert.deepEqual(app.redirects, []);
  assert.match(app.main.innerHTML, /restarting/);
  assert.equal(app.timers[0].delay, 3000);
  app.timers[0].callback();
  assert.deepEqual(app.redirects, ['/login']);
});
