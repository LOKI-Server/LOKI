const {test} = require('node:test');
const assert = require('node:assert/strict');
const path = require('node:path');
const {pathToFileURL} = require('node:url');
const modules = name => import(pathToFileURL(path.join(__dirname, '../static', name)).href);

test('device pairing and management belong to Account even without sync access', async () => {
  const {store} = await modules('lib/core.js');
  const {accountPage, pairPage} = await modules('pages/account.js');
  const {syncPage} = await modules('pages/sync.js');
  globalThis.location = {search: '?code=BCDF-GHJK'};
  store.session = {user: {id:'u',username:'reader',display_name:'Reader',bio:'',email:''},public_url:'https://loki.test'};
  store.state = {sessions:[],mfa:false,devices:[{id:'d',name:'Reader device',platform:'test',created:1,last_seen:1}]};
  const account = accountPage();
  assert.match(account,/id="devices"/);
  assert.match(account,/data-action="device-pair"/);
  assert.match(account,/data-action="device-rename"/);
  assert.match(account,/data-action="device-revoke"/);
  assert.match(account,/Reader device/);
  const pairing = pairPage();
  assert.match(pairing,/Back to your account/);
  assert.match(pairing,/BCDF-GHJK/);
  assert.doesNotMatch(pairing,/Back to Save sync/);
  assert.match(syncPage(),/Sync is not enabled/);
});

test('sync links to account device management instead of owning the pairing controls', async () => {
  const {store} = await modules('lib/core.js');
  const {syncPage} = await modules('pages/sync.js');
  store.state = {sync:{folders:[],quota:1024,usage:0,max_file_bytes:1024}};
  const page = syncPage();
  assert.match(page,/href="\/account"/);
  assert.doesNotMatch(page,/data-action="sync-pair"|data-action="sync-device-/);
});

test('account device approval explains sync and shared-file access and escapes device names', async () => {
  const {pairDialog} = await modules('pages/account.js');
  const body = {innerHTML:''};
  const title = {textContent:''};
  const dialog = {open:false,showModal(){this.open=true;}};
  globalThis.document = {querySelector: selector => selector === '#dialog-body' ? body : selector === '#dialog-title' ? title : dialog};
  pairDialog({kind:'account',service:'LOKI account',name:'<script>unsafe</script>',platform:'desktop',code:'BCDF-GHJK'});
  assert.match(body.innerHTML,/Save sync/);
  assert.match(body.innerHTML,/shared files/);
  assert.match(body.innerHTML,/data-action="device-approve"/);
  assert.doesNotMatch(body.innerHTML,/<script>/);
});
