const {test} = require('node:test');
const assert = require('node:assert/strict');
const path = require('node:path');
const {pathToFileURL} = require('node:url');

const modules = name => import(pathToFileURL(path.join(__dirname, '../static', name)).href);
const app = (id, status, extra = {}) => ({id, name: id, description: '', source: '', version: '2.0.0', url: 'https://example.com/a.loki-app', sha256: 'a'.repeat(64), size: 1024, sdk: '0.3.0', capabilities: ['database'], icon: false, status, reason: '', installed: null, ...extra});

test('the Browse page offers what the server says each app can do', async () => {
  const requests = [];
  globalThis.fetch = async url => {
    requests.push(url);
    return {ok: true, json: async () => ({url: 'https://loki-server.github.io/apps/', error: null, apps: [
      app('fresh', 'install'),
      app('older', 'update', {installed: '1.0.0'}),
      app('mine', 'replace', {installed: '2.0.0'}),
      app('done', 'installed', {installed: '2.0.0'}),
      app('future', 'unsupported', {reason: 'Update LOKI to run it.'}),
      app('<b>odd</b>', 'install', {source: 'https://github.com/x/y', icon: true}),
    ]})};
  };
  const {marketplacePage} = await modules('pages/marketplace.js');
  const html = await marketplacePage();
  const card = id => html.split('<article').find(c => c.includes(`data-key="${id}"`));
  assert.match(card('fresh'), /data-action="registry-install"[^>]*>.*Install</);
  assert.match(card('older'), /Update to v2\.0\.0/);
  assert.match(card('mine'), /An uploaded v2\.0\.0 uses this app ID/);
  assert.doesNotMatch(card('done'), /registry-install/);
  assert.match(card('future'), /Needs a newer LOKI.*Update LOKI to run it\./s);
  assert.doesNotMatch(card('future'), /registry-install/);
  assert.doesNotMatch(html, /<b>odd<\/b>/);
  assert.match(html, /\/api\/registry\/icons\/%3Cb%3Eodd%3C%2Fb%3E/);
  assert.match(html, /href="https:\/\/github\.com\/x\/y" target="_blank" rel="noopener noreferrer"/);
  // The list is reused for a minute rather than fetched on every render.
  await marketplacePage();
  assert.deepEqual(requests, ['/api/registry']);
});
