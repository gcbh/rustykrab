// Exercise the shipped submission handlers, with inert DOM and transport fixtures.
// No model, credentials, daemon or third-party packages are needed.
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import vm from 'node:vm';
import test from 'node:test';

const html = readFileSync(new URL('../crates/rustykrab-gateway/static/index.html', import.meta.url), 'utf8');
const script = [...html.matchAll(/<script>([\s\S]*?)<\/script>/g)][0][1];
new vm.Script(script); // Also catch syntax errors elsewhere in the bundled script.
function section(start, end) {
  const from = script.indexOf(start), to = script.indexOf(end, from);
  assert(from >= 0 && to > from, `Missing script section: ${start}`);
  return script.slice(from, to);
}
const handlers = [
  section('  const api = async', '  function forceReauth'),
  section('  function handleStreamEvent', '  const STATUS_ICONS'),
  section("  newChatBtn.addEventListener('click'", '  // ── Messages rendering'),
  section('  // ── Send Message', '  // ── Sidebar toggle'),
].join('\n');

function element() {
  const classes = new Set(), listeners = new Map();
  return {
    value: '', textContent: '', style: {}, disabled: false, scrollHeight: 20,
    classList: { toggle(name, on) { if (on) classes.add(name); else classes.delete(name); }, contains: name => classes.has(name) },
    addEventListener: (name, handler) => listeners.set(name, handler),
    dispatch: (name, event = {}) => listeners.get(name)(event),
    focus() {},
  };
}
function reply(events = [{ type: 'done', message: { role: 'assistant', content: 'Chat is working' } }]) {
  // Deliberately split frames across reads, including inside the terminal event.
  const payload = events.map(e => `data: ${JSON.stringify(e)}\n\n`).join('');
  const chunks = [payload.slice(0, 9), payload.slice(9)];
  return { ok: true, status: 200, body: { getReader: () => ({
    read: async () => chunks.length ? { done: false, value: new TextEncoder().encode(chunks.shift()) } : { done: true },
  }) } };
}
function client({ createStatus = 200, stream = () => reply(), createGate } = {}) {
  const calls = [], bubbles = [], selected = [];
  const c = {
    TextDecoder, console, conversations: [], activeConvId: null, activeMessages: [], isSending: false, streamBuffer: '',
    messageInput: element(), sendBtn: element(), newChatBtn: element(), composerError: element(),
    emptyState: element(), msgContainer: element(),
    access: { headers: () => ({ Authorization: 'Bearer fixture-only' }) },
    forceReauth() { c.reauth = true; },
    renderConversationList() {}, closeSidebar() {}, scrollToBottom() {},
    selectConversation: async id => { c.activeConvId = id; selected.push(id); },
    appendMessageBubble: (role, text) => bubbles.push({ role, text }),
    showTypingIndicator() { c.typing = true; }, removeTypingIndicator() { c.typing = false; },
    showAgentStatus() {}, removeAgentStatus() {}, updateStreamingMessage() {},
    finalizeStreamingMessage(message) {
      if (message) bubbles.push({ role: message.role, text: message.content });
      c.typing = false; c.isSending = false; c.updateSendButton();
    },
    fetch: async (path, options) => {
      calls.push({ path, options });
      if (path === '/api/conversations') {
        // Match the gateway's JSON extractor: empty JSON is an HTTP 400.
        let body;
        try { body = JSON.parse(options.body); } catch { return { ok: false, status: 400 }; }
        assert.deepEqual(body, {});
        assert.equal(options.headers['Content-Type'], 'application/json');
        if (createGate) await createGate;
        return { ok: createStatus === 200, status: createStatus, json: async () => ({ id: 'fixture-conversation', title: null }) };
      }
      assert.equal(path, '/api/conversations/fixture-conversation/messages/stream');
      assert.equal(JSON.parse(options.body).content, 'Connection check');
      assert.equal(options.headers.Authorization, 'Bearer fixture-only');
      return stream();
    },
  };
  vm.createContext(c); vm.runInContext(handlers, c);
  c.messageInput.value = 'Connection check';
  c.messageInput.dispatch('input');
  return { c, calls, bubbles, selected };
}

test('first message creates a conversation and sends a streamed reply', async () => {
  const { c, calls, bubbles } = client();
  assert.equal(c.sendBtn.disabled, false);
  await c.sendBtn.dispatch('click');
  assert.equal(calls.length, 2);
  assert.equal(bubbles.length, 2);
  assert.equal(bubbles[1].text, 'Chat is working');
  assert.equal(c.messageInput.value, '');
  assert.equal(c.isSending, false);
  assert.equal(c.composerError.style.display, 'none');
});

test('New conversation uses valid JSON and selects the created chat', async () => {
  const { c, calls, selected } = client();
  await c.newChatBtn.dispatch('click');
  assert.deepEqual(selected, ['fixture-conversation']);
  assert.equal(calls.length, 1);
  assert.equal(c.messageInput.value, 'Connection check');
  assert.equal(c.newChatBtn.disabled, false);
});

for (const control of ['sendBtn', 'newChatBtn']) {
  test(`${control} reports creation failure, keeps draft, and permits retry`, async () => {
    const { c, calls } = client({ createStatus: 503 });
    await c[control].dispatch('click');
    assert.equal(calls.length, 1);
    assert.equal(c.messageInput.value, 'Connection check');
    assert.equal(c.activeConvId, null);
    assert.match(c.composerError.textContent, /Could not start a conversation/);
    assert.equal(c.composerError.style.display, 'block');
    assert.equal(c.sendBtn.disabled, false);
    assert.equal(c.newChatBtn.disabled, false);
  });
}

test('creation in flight prevents duplicate Send/Enter/New conversation requests', async () => {
  let release;
  const gate = new Promise(resolve => { release = resolve; });
  const { c, calls } = client({ createGate: gate });
  const pending = c.sendBtn.dispatch('click');
  assert.equal(c.sendBtn.disabled, true);
  assert.equal(c.newChatBtn.disabled, true);
  await c.sendBtn.dispatch('click');
  c.messageInput.dispatch('keydown', { key: 'Enter', preventDefault() {} });
  await c.newChatBtn.dispatch('click');
  assert.equal(calls.length, 1);
  release(); await pending;
  assert.equal(calls.length, 2);
});

for (const [name, stream, expected] of [
  ['HTTP failure', () => ({ ok: false, status: 503 }), /interrupted/],
  ['SSE error', () => reply([{ type: 'error', error: 'fixture failure' }]), /failed/],
  ['premature EOF', () => reply([{ type: 'text', delta: 'Partial response' }]), /interrupted/],
]) {
  test(`${name} is visible and restores submission controls`, async () => {
    const { c } = client({ stream });
    await c.sendBtn.dispatch('click');
    assert.equal(c.isSending, false);
    assert.equal(c.typing, false);
    assert.match(c.composerError.textContent, expected);
    c.messageInput.value = 'Connection check'; c.messageInput.dispatch('input');
    assert.equal(c.sendBtn.disabled, false);
    assert.equal(c.newChatBtn.disabled, false);
  });
}
