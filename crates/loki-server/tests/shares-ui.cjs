const {test} = require('node:test');
const assert = require('node:assert/strict');
const path = require('node:path');
const {pathToFileURL} = require('node:url');

const modules = name => import(pathToFileURL(path.join(__dirname, '../static', name)).href);
const define = (name, value) => Object.defineProperty(globalThis, name, {value, configurable: true, writable: true});

async function browser() {
  const {store} = await modules('lib/core.js');
  const manage = await modules('pages/manage.js');
  const {onClick} = await modules('lib/actions.js');
  const body = {innerHTML: ''};
  const dialog = {open: false, showModal() { this.open = true; }, addEventListener() {}};
  define('location', {pathname: '/shares', href: 'http://loki.test/shares'});
  define('document', {querySelector: selector => selector === '#dialog-body' ? body : dialog});
  define('fetch', async url => {
    assert.match(url, /^\/api\/folders\?path=/);
    return {ok: true, status: 200, json: async () => ({path: 'old', parent: null, folders: []})};
  });
  store.session = {user: {owner: true}, csrf: 'csrf'};
  store.state = {resources: [{id: 'saved', kind: 'share', name: 'Saved share', root: 'old', excluded_extensions: ['bak'], visibility: 'restricted'}], grants: [], groups: [], users: []};
  return {body, manage, async click(action, form, selectedPath) {
    const button = {dataset: {action, path: selectedPath}, closest: () => form};
    await onClick({preventDefault() {}, target: {
      closest: selector => selector === '[data-action]' ? button : null
    }});
  }};
}

test('new and existing shares expose editable extension exclusions', async () => {
  const app = await browser();
  const created = app.manage.shareForm();
  assert.match(created, /name="excluded_extensions"/);
  assert.match(created, /placeholder=".tmp, .bak, .psd, .tar.gz"/);
  app.manage.shareManage('saved');
  assert.match(app.body.innerHTML, /name="excluded_extensions"[^>]*value=".bak"/);
});

for (const id of ['', 'saved']) {
  test(`folder selection preserves unsaved exclusions for ${id ? 'existing' : 'new'} shares`, async () => {
    const app = await browser();
    const form = {dataset: {id}, elements: {
      name: {value: 'Draft name'}, root: {value: 'old'},
      excluded_extensions: {value: '.TMP, .tar.gz'}
    }};
    await app.click('folder-picker', form);
    await app.click('folder-select', null, 'new-folder');
    assert.match(app.body.innerHTML, /name="root"[^>]*value="new-folder"/);
    assert.match(app.body.innerHTML, /name="name"[^>]*value="Draft name"/);
    assert.match(app.body.innerHTML, /name="excluded_extensions"[^>]*value=".TMP, .tar.gz"/);
  });
}
