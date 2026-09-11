import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync, mkdtempSync, rmSync, statSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { createServer } from 'node:net';
import { stripTypeScriptTypes } from 'node:module';
import { spawnSync } from 'node:child_process';
import vm from 'node:vm';

if (!vm.SourceTextModule) {
  const res = spawnSync(
    process.execPath,
    ['--experimental-vm-modules', ...process.execArgv, ...process.argv.slice(1)],
    { stdio: 'inherit', env: process.env }
  );
  process.exit(res.status ?? 0);
}

// Exercise the bundled source, without needing Pi credentials or a model.
const context = vm.createContext({ process, Buffer, console });
const source = stripTypeScriptTypes(readFileSync(new URL('../src/pi/noches-cua.ts', import.meta.url), 'utf8'));
const module = new vm.SourceTextModule(source, { context });
await module.link(async (specifier) => {
  let exports;
  if (specifier === 'typebox') {
    exports = { Type: { Object: x => x, String: x => x, Optional: x => x, Record: () => ({}), Any: () => ({}) } };
  } else if (specifier === '@earendil-works/pi-coding-agent') {
    exports = {
      DEFAULT_MAX_BYTES: 50 * 1024, DEFAULT_MAX_LINES: 2000,
      truncateHead(text, { maxBytes, maxLines }) {
        const lines = text.split('\n');
        const kept = lines.slice(0, maxLines).join('\n');
        const content = Buffer.from(kept).subarray(0, maxBytes).toString('utf8');
        return { content, truncated: Buffer.byteLength(text) > maxBytes || lines.length > maxLines };
      },
    };
  } else { exports = await import(specifier); }
  return new vm.SyntheticModule(Object.keys(exports), function () {
    for (const [name, value] of Object.entries(exports)) this.setExport(name, value);
  }, { context });
});
await module.evaluate();

async function withServer(handler, body) {
  const dir = mkdtempSync(join(tmpdir(), 'noches-adapter-test-'));
  const socket = join(dir, 'bridge.sock');
  const previous = process.env.NOCHES_CUA_SOCKET;
  const peers = new Set();
  const server = createServer(peer => {
    peers.add(peer); peer.on('close', () => peers.delete(peer));
    handler(peer);
  });
  await new Promise(resolve => server.listen(socket, resolve));
  process.env.NOCHES_CUA_SOCKET = socket;
  try { await body(); }
  finally {
    if (previous === undefined) delete process.env.NOCHES_CUA_SOCKET;
    else process.env.NOCHES_CUA_SOCKET = previous;
    for (const peer of peers) peer.destroy();
    await new Promise(resolve => server.close(resolve));
    rmSync(dir, { recursive: true, force: true });
  }
}

test('adapter preserves split UTF-8, images, structured fields and driver refusals', { timeout: 5000 }, async () => {
  const hooks = new Map(); let tool; let active = ['read', 'cua', 'noches_cua'];
  module.namespace.default({
    on(name, handler) { hooks.set(name, handler); },
    getActiveTools() { return active; }, setActiveTools(names) { active = names; },
    registerTool(definition) { tool = definition; },
  });
  hooks.get('session_start')();
  assert.equal(active.join(','), 'read,noches_cua');
  assert.equal(hooks.get('tool_call')({ toolName: 'cua' }).block, true);
  assert.equal(hooks.get('tool_call')({ toolName: 'read' }), undefined);
  await withServer(peer => peer.once('data', request => {
    assert.equal(JSON.parse(request).action, 'get_window_state');
    const data = Buffer.from(JSON.stringify({ isError: true,
      content: [{ type: 'text', text: '你好' }, { type: 'image', data: 'cG5n', mimeType: 'image/png' }],
      structuredContent: { element_token: 's1:3' } }) + '\n');
    const split = data.indexOf(Buffer.from('你')) + 1;
    peer.write(data.subarray(0, split));
    setImmediate(() => peer.write(data.subarray(split)));
  }), async () => {
    const result = await tool.execute('t', { action: 'get_window_state', args: {} });
    assert.match(result.content[0].text, /你好/);
    assert.match(result.content[0].text, /s1:3/);
    assert.equal(result.content[1].data, 'cG5n');
    assert.equal(result.details.structuredContent.element_token, 's1:3');
    assert.equal(hooks.get('tool_result')({ toolName: 'noches_cua', details: result.details }).isError, true);
  });
});

test('abort disconnects the caller and EOF rejects rather than hanging', { timeout: 5000 }, async () => {
  await withServer(peer => peer.once('data', () => peer.end()), async () => {
    await assert.rejects(module.namespace.callBridge('click', {}), /closed|disconnected/);
  });
  let arrived;
  const received = new Promise(resolve => { arrived = resolve; });
  await withServer(peer => peer.once('data', arrived), async () => {
    const controller = new AbortController();
    const pending = module.namespace.callBridge('click', {}, controller.signal);
    const rejected = assert.rejects(pending, /cancelled/);
    await received;
    controller.abort();
    await rejected;
  });
});

test('truncated text and structured content remain in a private file', () => {
  const result = module.namespace.toolResult({content: [{type:'text', text:'x'.repeat(60 * 1024)}], structuredContent: { element_token:'last-token' }}, 'get_window_state');
  const path = result.content[0].text.match(/Full text and structured result: ([^\]]+)/)[1];
  try {
    assert.match(readFileSync(path, 'utf8'), /last-token/);
    assert.equal(statSync(path).mode & 0o777, 0o600);
    assert.equal(statSync(join(path, '..')).mode & 0o777, 0o700);
  } finally { rmSync(join(path, '..'), { recursive: true, force: true }); }
});

test('help is compact and does not spill its schema catalog to disk', () => {
  const tools = Array.from({ length: 200 }, (_, index) => ({ name: `action_${index}`, schema: 'x'.repeat(1000) }));
  const result = module.namespace.toolResult({
    content: [{ type: 'text', text: 'large catalog' }],
    structuredContent: { tools },
  }, 'help');
  assert.match(result.content[0].text, /Available actions: action_0/);
  assert.match(result.content[0].text, /synthetic agent cursor/);
  assert.match(result.content[0].text, /Do not parse help output with shell commands/);
  assert.doesNotMatch(result.content[0].text, /Full text and structured result/);
});

test('disruptive desktop and foreground routes require explicit user opt-in', () => {
  assert.throws(
    () => module.namespace.bridgeArgs('click', { target: { kind: 'desktop', display_id: 'DP-2' } }),
    /real pointer or change keyboard focus/,
  );
  assert.throws(
    () => module.namespace.bridgeArgs('type_text', { delivery_mode: 'foreground' }),
    /real pointer or change keyboard focus/,
  );
  assert.equal(
    JSON.stringify(module.namespace.bridgeArgs('click', {
      target: { kind: 'desktop', display_id: 'DP-2' }, allow_user_input_disruption: true,
    })),
    JSON.stringify({ target: { kind: 'desktop', display_id: 'DP-2' } }),
  );
  assert.equal(
    JSON.stringify(module.namespace.bridgeArgs('browser_navigate', { target_id: 'b1', tab_id: 't1', url: 'https://x.com' })),
    JSON.stringify({ target_id: 'b1', tab_id: 't1', url: 'https://x.com' }),
  );
});

test('get_window_state shapes structured elements, excludes tree_markdown and suppresses redundant markdown text', () => {
  const result = module.namespace.toolResult({
    content: [{ type: 'text', text: 'window_id=1 pid=2 elements=1\n\n[0] button "OK"' }],
    structuredContent: {
      element_count: 1,
      elements: [{ element_index: 0, role: 'button', label: 'OK' }],
      tree_markdown: '[0] button "OK"',
    },
  }, 'get_window_state');
  assert.doesNotMatch(result.content[0].text, /window_id=1/);
  assert.doesNotMatch(result.content[0].text, /tree_markdown/);
  assert.match(result.content[0].text, /"elements":\[\{"element_index":0,"role":"button","label":"OK"\}\]/);
  assert.match(result.content[0].text, /"element_count":1/);
  // Programmatic details preserve the driver's full structured content.
  assert.equal(result.details.structuredContent.tree_markdown, '[0] button "OK"');
});

test('get_window_state preserves images and refusal/error text', () => {
  const result = module.namespace.toolResult({
    isError: true,
    content: [
      { type: 'text', text: 'Refusal: window lookup failed' },
      { type: 'image', data: 'c2NyZWVu', mimeType: 'image/png' },
    ],
    structuredContent: { degraded: true, degraded_reason: 'unresolved' },
  }, 'get_window_state');
  assert.match(result.content[0].text, /Refusal: window lookup failed/);
  assert.match(result.content[0].text, /"degraded_reason":"unresolved"/);
  assert.equal(result.content[1].type, 'image');
  assert.equal(result.content[1].data, 'c2NyZWVu');
  assert.equal(result.details.driverIsError, true);
});

test('get_window_state preserves full raw output in secure file on actual truncation', () => {
  const longMarkdown = 'window_id=1 pid=2 elements=1000\n' + '[0] row\n'.repeat(3000);
  const elements = Array.from({ length: 1500 }, (_, i) => ({ element_index: i, role: 'row', label: `Item ${i}` }));
  const result = module.namespace.toolResult({
    content: [{ type: 'text', text: longMarkdown }],
    structuredContent: {
      element_count: 1500,
      elements,
      tree_markdown: 'RAW_ACCESSIBILITY_TREE_MARKDOWN_CONTENT',
    },
  }, 'get_window_state');
  const path = result.content[0].text.match(/Full text and structured result: ([^\]]+)/)[1];
  try {
    const rawSaved = readFileSync(path, 'utf8');
    assert.match(rawSaved, /RAW_ACCESSIBILITY_TREE_MARKDOWN_CONTENT/);
    assert.match(rawSaved, /window_id=1 pid=2 elements=1000/);
    assert.equal(statSync(path).mode & 0o777, 0o600);
    assert.equal(statSync(join(path, '..')).mode & 0o777, 0o700);
  } finally {
    rmSync(join(path, '..'), { recursive: true, force: true });
  }
});

test('promptGuidelines carry managed keyboard, browser and profile guidance', () => {
  const guidelines = [];
  module.namespace.default({
    on() {}, getActiveTools: () => [], setActiveTools() {},
    registerTool(definition) { guidelines.push(...definition.promptGuidelines); },
  });
  const all = guidelines.join('\n');
  // Isolated background keyboard: focus child first, then minimal keyboard args.
  const kb = guidelines.find(line => line.includes('isolated background keyboard'));
  assert.ok(kb, 'isolated background keyboard guidance exists');
  assert.match(kb, /click the child/);
  assert.match(kb, /element token or coordinates/);
  assert.match(kb, /only pid\/window_id\/text/);
  assert.match(kb, /only pid\/window_id\/key/);
  assert.match(kb, /rejected/);
  assert.doesNotMatch(kb, /delivery_mode/);
  // Omnibox focus uses an explicit keys/modifiers shape with pid/window_id.
  const omnibox = guidelines.find(line => line.includes('omnibox'));
  assert.ok(omnibox, 'omnibox guidance exists');
  assert.match(omnibox, /hotkey \(keys:\["ctrl","l"\]/);
  assert.match(omnibox, /key:"l", modifiers:\["ctrl"\]/);
  assert.match(omnibox, /pid\/window_id/);
  assert.doesNotMatch(omnibox, /key ctrl\+l/i);
  // Profile guidance: isolated_new default, existing_profile only when needed.
  const profile = guidelines.find(line => line.includes('isolated_new'));
  assert.ok(profile, 'browser_prepare profile guidance exists');
  assert.match(profile, /isolated_new/);
  assert.match(profile, /existing_profile/);
  assert.match(profile, /logged-in profile data/);
  // Compositor exception and synthetic cursor text still present.
  assert.match(all, /hl\.dispatch/);
  assert.match(all, /synthetic agent cursor/);
});

test('describe formats multiple schemas without help compaction', () => {
  const result = module.namespace.toolResult({
    content: [{ type: 'text', text: 'Requested managed computer-use schemas.' }],
    structuredContent: {
      tools: [
        { name: 'click', inputSchema: { type: 'object' } },
        { name: 'list_windows', inputSchema: { type: 'object' } },
      ],
    },
  }, 'describe');
  assert.match(result.content[0].text, /Requested managed computer-use schemas\./);
  assert.match(result.content[0].text, /"name":"click"/);
  assert.match(result.content[0].text, /"name":"list_windows"/);
  assert.doesNotMatch(result.content[0].text, /Available actions:/);
});

test('other actions preserve full fidelity', () => {
  const result = module.namespace.toolResult({
    content: [{ type: 'text', text: 'Window list below:' }],
    structuredContent: { windows: [{ pid: 100, title: 'Editor' }] },
  }, 'list_windows');
  assert.match(result.content[0].text, /Window list below:/);
  assert.match(result.content[0].text, /"title":"Editor"/);
});
