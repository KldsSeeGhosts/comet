import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync, mkdtempSync, rmSync, statSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { createServer } from 'node:net';
import { stripTypeScriptTypes } from 'node:module';
import vm from 'node:vm';

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
