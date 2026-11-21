const {test} = require('node:test');
const assert = require('node:assert/strict');
const path = require('node:path');
const {pathToFileURL} = require('node:url');

const modules = name => import(pathToFileURL(path.join(__dirname, '../static', name)).href);

test('html escapes values and keeps nested markup', async () => {
  const {html, raw} = await modules('lib/html.js');
  const name = '<img src=x onerror=alert(1)>';
  const out = String(html`<p title="${name}">${name}${html`<b>${'&'}</b>`}${[1, html`<i>2</i>`]}${null}${false}</p>`);
  assert.equal(out, '<p title="&lt;img src=x onerror=alert(1)&gt;">&lt;img src=x onerror=alert(1)&gt;<b>&amp;</b>1<i>2</i></p>');
  assert.equal(String(raw('<svg/>')), '<svg/>');
});

test('joining markup with + fails instead of escaping it later', async () => {
  const {html} = await modules('lib/html.js');
  assert.throws(() => html`<p></p>` + html`<p></p>`, TypeError);
  assert.equal(`${html`<p></p>`}`, '<p></p>');
});

test('links cannot run script', async () => {
  const {safeUrl, attrs} = await modules('lib/html.js');
  for (const bad of ['javascript:alert(1)', ' JavaScript:alert(1)', 'data:text/html,x', 'vbscript:x']) assert.equal(safeUrl(bad), '#', bad);
  for (const good of ['/sync', '?tab=code', '#devices', 'https://example.com/a', 'mailto:a@b.c', 'docs/readme']) assert.equal(safeUrl(good), good, good);
  assert.throws(() => attrs({'onclick x': 1}));
  assert.equal(String(attrs({href: '"x"', hidden: true, title: null, disabled: false})), ' href="&quot;x&quot;" hidden');
});

test('components escape what they are given', async () => {
  const {Button, LinkButton, Row, Field} = await modules('lib/ui.js');
  const evil = '"><script>x</script>';
  for (const out of [Button({label: evil, action: 'a', id: evil, data: {path: evil}}), LinkButton({label: evil, href: 'javascript:alert(1)'}), Row({title: evil, href: evil, meta: evil}), Field({name: 'n', label: evil, value: evil})].map(String)) {
    assert.ok(!out.includes('<script>'), out);
  }
  assert.match(String(LinkButton({label: 'x', href: 'javascript:alert(1)'})), /href="#"/);
  assert.match(String(Button({label: 'Go', action: 'go', id: '7', data: {keepCurrent: 'yes'}})), /data-action="go" data-id="7" data-keep-current="yes"/);
});
