const {test} = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

const conflict = proof => ({requires_confirmation: 'startup', startup: {
  existing_executable: 'F:\\Portable\\loki-server.exe',
  current_executable: 'C:\\Apps\\LOKI\\loki-server.exe',
  data_directory: 'F:\\Servers\\LOKI', confirmation: proof
}});
function browser(responses) {
  const calls = [], redirects = [], nodes = new Map();
  const main = {innerHTML: 'unsaved setup form'};
  nodes.set('#main', main);
  nodes.set('#dialog-body', {innerHTML: ''});
  const dialog = {listeners: new Map(), open: false,
    showModal() {this.open = true;},
    addEventListener(event, handler) {this.listeners.set(event, handler);},
    removeEventListener(event, handler) {if (this.listeners.get(event) === handler) this.listeners.delete(event);},
    close() {this.open = false; const closed = this.listeners.get('close'); this.listeners.delete('close'); closed?.();}
  };
  nodes.set('#dialog', dialog);
  for (const id of ['#startup-replace', '#startup-cancel']) {
    nodes.set(id, {addEventListener(event, handler) {this[event] = handler;}});
  }
  const context = vm.createContext({FormData,
    document: {querySelector: selector => {assert.ok(nodes.has(selector), selector); return nodes.get(selector);}},
    location: {pathname: '/setup', assign: url => redirects.push(url)},
    sessionStorage: {setItem() {}}, setTimeout() {},
    fetch: async (url, options) => {
      calls.push({url, options});
      assert.ok(responses.length, 'unexpected request');
      const body = responses.shift();
      return {ok: true, status: 200, json: async () => body};
    }
  });
  const source = fs.readFileSync(path.join(__dirname, '../static/app.js'), 'utf8');
  vm.runInContext(source.slice(0, source.indexOf('function nav()')), context);
  vm.runInContext('session={csrf:"csrf"}', context);
  return {context, calls, redirects, nodes, main, dialog,
    submit: () => vm.runInContext('submitSetup({token:"setup-token",username:"owner",password:"test-password",startup:true})', context),
    ready: () => new Promise(resolve => setImmediate(resolve)),
    click: id => nodes.get(id).click()
  };
}

test('canceling portable startup replacement keeps setup and makes no retry', async () => {
  const app = browser([conflict('first')]);
  const pending = app.submit();
  await app.ready();
  assert.equal(app.dialog.open, true);
  assert.match(app.nodes.get('#dialog-body').innerHTML, /F:\\Portable\\loki-server.exe/);
  app.click('#startup-cancel');
  await pending;
  assert.equal(app.calls.length, 1);
  assert.deepEqual(app.redirects, []);
  assert.equal(app.main.innerHTML, 'unsaved setup form');
});

test('replacing startup resubmits setup with the reviewed confirmation', async () => {
  const app = browser([conflict('reviewed'), {ok: true}]);
  const pending = app.submit();
  await app.ready();
  app.click('#startup-replace');
  await pending;
  assert.equal(app.calls.length, 2);
  assert.equal(JSON.parse(app.calls[0].options.body).startup_confirmation, undefined);
  assert.equal(JSON.parse(app.calls[1].options.body).startup_confirmation, 'reviewed');
  assert.deepEqual(app.redirects, ['/login']);
});

test('a changed startup entry requires a new review before settings are saved', async () => {
  const app = browser([conflict('old'), conflict('changed'), {ok: true}]);
  const pending = vm.runInContext('submitStartupAware(confirmation=>act("settings","",{startup:true,startup_confirmation:confirmation}))', app.context);
  await app.ready();
  app.click('#startup-replace');
  await app.ready();
  assert.equal(app.calls.length, 2);
  assert.equal(app.dialog.open, true);
  app.click('#startup-replace');
  assert.equal((await pending).ok, true);
  assert.deepEqual(app.calls.map(call => JSON.parse(call.options.body).data.startup_confirmation), [undefined, 'old', 'changed']);
});

test('closing the confirmation dialog cancels and paths are escaped', async () => {
  const value = conflict('proof');
  value.startup.existing_executable = '<img src=x onerror=alert(1)>';
  const app = browser([value]);
  const pending = app.submit();
  await app.ready();
  const html = app.nodes.get('#dialog-body').innerHTML;
  assert.ok(html.includes('&lt;img'));
  assert.ok(!html.includes('<img'));
  app.dialog.close();
  await pending;
  assert.equal(app.calls.length, 1);
  assert.deepEqual(app.redirects, []);
});

test('a restored enabled flag does not show another executable as this copy startup', () => {
  const app = browser([]);
  vm.runInContext('state={startup:{needs_confirmation:true,registered_executable:"F:\\Portable\\loki-server.exe"}}', app.context);
  const html = vm.runInContext('startupControl({startup:true})', app.context);
  assert.ok(!html.includes(' checked'));
  assert.ok(html.includes('another LOKI copy'));
});
