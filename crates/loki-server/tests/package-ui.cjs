const {test} = require('node:test');
const assert = require('node:assert/strict');
const path = require('node:path');
const {pathToFileURL} = require('node:url');

const modules = name => import(pathToFileURL(path.join(__dirname, '../static', name)).href);

test('the package preview asks for a rebuild exactly when the server would refuse the app', async () => {
  const {store} = await modules('lib/core.js');
  const {sdkProblem} = await modules('lib/package.js');
  store.state = {loki_version: '0.3.2', oldest_sdk: '0.3.0'};
  assert.match(sdkProblem(undefined), /older than 0\.3\. Rebuild it with loki-sdk 0\.3\.0/);
  assert.match(sdkProblem('soon'), /not a valid version/);
  assert.match(sdkProblem('0.2.9'), /no longer runs/);
  assert.equal(sdkProblem('0.3.0'), '');
  assert.equal(sdkProblem('0.3.2'), '');
  assert.match(sdkProblem('0.3.3'), /needs LOKI 0\.3\.3 or newer/);
  assert.match(sdkProblem('0.4.0'), /needs LOKI 0\.4\.0 or newer/);
  assert.match(sdkProblem('1.0.0'), /needs LOKI 1\.0\.0 or newer/);
});
