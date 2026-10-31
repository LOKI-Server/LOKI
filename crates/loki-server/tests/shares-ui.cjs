const {test} = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

function browser() {
  const body = {innerHTML: ''};
  const dialog = {showModal() {}, addEventListener() {}};
  const handlers = {};
  const context = vm.createContext({
    location: {pathname: '/shares'},
    document: {
      querySelector: selector => selector === '#dialog-body' ? body : dialog,
      addEventListener: (event, handler) => { handlers[event] = handler; }
    }
  });
  const script = fs.readFileSync(path.join(__dirname, '../static/app.js'), 'utf8');
  vm.runInContext(script.slice(0, script.indexOf('load().catch')), context);
  vm.runInContext('session={user:{owner:true}};state={resources:[{id:"saved",name:"Saved share",root:"old",excluded_extensions:["bak"]}]};folderPicker=async()=>{};accessPanel=()=>"";', context);
  return {body, context, async click(action, form, selectedPath) {
    const button = {dataset: {action, path: selectedPath}, closest: () => form};
    await handlers.click({preventDefault() {}, target: {
      closest: selector => selector === '[data-action]' ? button : null
    }});
  }};
}

test('new and existing shares expose editable extension exclusions', () => {
  const app = browser();
  const created = vm.runInContext('shareForm()', app.context);
  assert.match(created, /name="excluded_extensions"/);
  assert.match(created, /placeholder=".tmp, .bak, .psd, .tar.gz"/);
  vm.runInContext('shareManage("saved")', app.context);
  assert.match(app.body.innerHTML, /name="excluded_extensions"[^>]*value=".bak"/);
});

for (const id of ['', 'saved']) {
  test(`folder selection preserves unsaved exclusions for ${id ? 'existing' : 'new'} shares`, async () => {
    const app = browser();
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
