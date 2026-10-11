// Exercise monitor notification links against the shipped connection handler.
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import vm from 'node:vm';
import test from 'node:test';

const source = readFileSync(new URL('../crates/rustykrab-gateway/static/monitor.js', import.meta.url), 'utf8');
new vm.Script(source);
const linked = source.slice(source.indexOf('  const linkedWork ='), source.indexOf('  const node ='));
const handler = source.slice(source.indexOf('  async function connect('), source.indexOf('  function stat('));
assert(linked.includes('URLSearchParams') && handler.includes('showDetail'));

function client(search, authenticated) {
  const elements = new Map(), opened = [], calls = [];
  const c = {
    URLSearchParams, window: { location: { search } },
    snapshot: null, generation: 0, pending: false, lastQuestions: '', paused: false, interval: null,
    $: id => {
      if (!elements.has(id)) elements.set(id, { value: '', textContent: '', disabled: false, setAttribute() {} });
      return elements.get(id);
    },
    access: {
      connect: async automatic => { calls.push(automatic); return { authenticated, tailscale_enabled: true }; },
      message: () => '',
    },
    clearInterval() {}, setInterval: () => 1, document: { hidden: false },
    refresh: async () => { c.snapshot = { work: {} }; },
    showDetail: async id => { opened.push(id); },
  };
  vm.createContext(c); vm.runInContext(linked + handler, c);
  return { c, opened, calls };
}

test('authenticated notification link opens that execution after monitor connects', async () => {
  const id = 'c453761f-9b75-4952-80ec-4a6b702f8716';
  const { c, opened } = client('?work=' + id, true);
  await c.connect(true);
  assert.deepEqual(opened, [id]);
});

test('notification link waits for authentication and survives manual connection', async () => {
  const { c, opened } = client('?work=target-item', false);
  await c.connect(true);
  assert.deepEqual(opened, []);
  assert.equal(c.snapshot, null);
  await c.connect(false, async () => ({ authenticated: true }));
  assert.deepEqual(opened, ['target-item']);
});

test('normal monitor visits do not open an execution automatically', async () => {
  const { c, opened } = client('', true);
  await c.connect(true);
  assert.deepEqual(opened, []);
});

test('work identifier is decoded as data rather than executed', async () => {
  const { c, opened } = client('?work=%3Cscript%3Ealert(1)%3C%2Fscript%3E', true);
  await c.connect(true);
  assert.deepEqual(opened, ['<script>alert(1)</script>']);
});
