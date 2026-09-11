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

function truncatedPaths(text) {
  const match = text.match(/\[Output truncated\. Full text: ([^;]+); structured result: ([^\]]+)\]/);
  assert.ok(match, `truncation pointer missing in ${text}`);
  return { textPath: match[1], jsonPath: match[2] };
}

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
    assert.equal(result.details.driverOutcome, 'error');
    assert.equal(hooks.get('tool_result')({ toolName: 'noches_cua', details: result.details }).isError, true);
  });
});

test('nested browser refusal becomes a Pi execution error without losing evidence', { timeout: 5000 }, async () => {
  // Exact shape from the audited noches_session_export refusal: the driver
  // returned status=refused with the refusal nested and no execution-error flag.
  const refusal = {
    status: 'refused',
    refusal: {
      code: 'browser_consent_required',
      message: 'this standalone browser profile requires explicit existing-profile approval before Cua can inspect its DevTools endpoint',
      detail: {
        next_action: 'browser_prepare',
        reason: 'consumer_profile_endpoint_requires_grant',
        supported_strategies: ['existing_profile'],
      },
    },
  };
  const result = module.namespace.toolResult({
    content: [{ type: 'text', text: 'browser refusal payload' }],
    structuredContent: refusal,
  }, 'get_browser_state');
  assert.equal(module.namespace.classifyOutcome({ structuredContent: refusal }), 'refused');
  assert.equal(result.details.driverOutcome, 'refused');
  assert.equal(result.details.driverIsError, true);
  assert.deepEqual(result.details.structuredContent, refusal);
  assert.match(result.content[0].text, /Computer-use refused \(browser_consent_required\): this standalone browser profile/);
  assert.match(result.content[0].text, /Reason: consumer_profile_endpoint_requires_grant\./);
  assert.match(result.content[0].text, /Next action: browser_prepare\./);
  assert.match(result.content[0].text, /Approval: supported_strategies=existing_profile\./);
  assert.match(result.content[0].text, /"code":"browser_consent_required"/);
  assert.match(result.content[0].text, /"next_action":"browser_prepare"/);
  assert.match(result.content[0].text, /"supported_strategies":\["existing_profile"\]/);
  // The tool_result hook surfaces it as an execution error; no false success.
  const hooks = new Map();
  module.namespace.default({ on(name, handler) { hooks.set(name, handler); }, getActiveTools: () => [], setActiveTools() {}, registerTool() {} });
  assert.equal(hooks.get('tool_result')({ toolName: 'noches_cua', details: result.details }).isError, true);
  assert.equal(hooks.get('tool_result')({ toolName: 'noches_cua', details: { driverIsError: false } }), undefined);
});

test('effect refused classifies as refusal even with the driver error flag', () => {
  // Exact shape from the audited effect=refused trace; the driver had already
  // set isError, so the classifier must keep the exact refusal classification.
  const structured = {
    code: 'background_unavailable',
    detail: 'client_not_qualified',
    effect: 'refused',
    ok: false,
    reason: 'client_not_qualified',
    route: 'synthetic_events',
    verified: false,
  };
  assert.equal(module.namespace.classifyOutcome({ isError: true, structuredContent: structured }), 'refused');
  const result = module.namespace.toolResult({ isError: true, structuredContent: structured }, 'click');
  assert.equal(result.details.driverOutcome, 'refused');
  assert.equal(result.details.driverIsError, true);
  assert.deepEqual(result.details.structuredContent, structured);
  assert.match(result.content[0].text, /Computer-use refused \(background_unavailable\)/);
  assert.match(result.content[0].text, /Reason: client_not_qualified\./);
});

test('classifier precedence is consistent for isError and structured status', () => {
  const classify = (result) => module.namespace.classifyOutcome(result);
  // Structured refusals outrank the generic execution-error flag.
  assert.equal(classify({ structuredContent: { status: 'refused' } }), 'refused');
  assert.equal(classify({ isError: true, structuredContent: { status: 'refused' } }), 'refused');
  assert.equal(classify({ isError: true, structuredContent: { effect: 'refused' } }), 'refused');
  assert.equal(classify({ isError: true, structuredContent: { refused: true } }), 'refused');
  // Explicit error and cancelled statuses keep their classification.
  assert.equal(classify({ isError: true, structuredContent: { status: 'error' } }), 'error');
  assert.equal(classify({ structuredContent: { status: 'cancelled' } }), 'cancelled');
  assert.equal(classify({ isError: true, structuredContent: { status: 'cancelled' } }), 'cancelled');
  // Uncertain deliveries stay non-errors only without the driver error flag.
  for (const structured of [{ status: 'partial' }, { status: 'unknown' }, { status: 'unverifiable' }, { effect: 'unverifiable' }]) {
    assert.equal(classify({ structuredContent: structured }), structured.status ?? 'unverifiable', JSON.stringify(structured));
    assert.equal(classify({ isError: true, structuredContent: structured }), 'error', JSON.stringify(structured));
  }
  // A bare driver error flag is still an error.
  assert.equal(classify({ isError: true }), 'error');
});

test('partial, unknown and unverifiable deliveries stay non-errors', () => {
  const cases = [
    { status: 'partial', delivery_id: 'd1' },
    { status: 'unknown' },
    { status: 'unverifiable' },
    { effect: 'unverifiable' },
  ];
  for (const structured of cases) {
    const outcome = module.namespace.classifyOutcome({ structuredContent: structured });
    assert.notEqual(outcome, 'refused', JSON.stringify(structured));
    assert.notEqual(outcome, 'error', JSON.stringify(structured));
    const result = module.namespace.toolResult({ structuredContent: structured }, 'click');
    assert.equal(result.details.driverIsError, false, JSON.stringify(structured));
    assert.doesNotMatch(result.content[0].text, /Computer-use refused/);
    assert.deepEqual(result.details.structuredContent, structured);
  }
  // An explicitly flagged driver error still normalizes to a Pi error.
  const failed = module.namespace.toolResult({ isError: true, structuredContent: { status: 'failed' } }, 'click');
  assert.equal(failed.details.driverIsError, true);
  assert.equal(failed.details.driverOutcome, 'error');
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

test('truncated text and structured content remain in private split files', () => {
  const result = module.namespace.toolResult({content: [{type:'text', text:'x'.repeat(60 * 1024)}], structuredContent: { element_token:'last-token' }}, 'get_window_state');
  const { textPath, jsonPath } = truncatedPaths(result.content[0].text);
  try {
    const rawText = readFileSync(textPath, 'utf8');
    assert.match(rawText, /^x+/);
    assert.doesNotMatch(rawText, /last-token/, 'structured data belongs in result.json, not result.txt');
    const parsed = JSON.parse(readFileSync(jsonPath, 'utf8'));
    assert.equal(parsed.action, 'get_window_state');
    assert.equal(parsed.structuredContent.element_token, 'last-token');
    assert.equal(statSync(textPath).mode & 0o777, 0o600);
    assert.equal(statSync(jsonPath).mode & 0o777, 0o600);
    assert.equal(statSync(join(textPath, '..')).mode & 0o777, 0o700);
  } finally { rmSync(join(textPath, '..'), { recursive: true, force: true }); }
});

test('help is compact and does not spill its schema catalog to disk', () => {
  const tools = Array.from({ length: 200 }, (_, index) => ({ name: `action_${index}`, schema: 'x'.repeat(1000) }));
  const result = module.namespace.toolResult({
    content: [{ type: 'text', text: 'large catalog' }],
    structuredContent: { tools, driver: { instructions: 'RAW_SERVER_INSTRUCTIONS_PROSE' } },
  }, 'help');
  assert.match(result.content[0].text, /Available actions: action_0/);
  assert.match(result.content[0].text, /synthetic agent cursor/);
  assert.match(result.content[0].text, /Do not parse help output with shell commands/);
  assert.doesNotMatch(result.content[0].text, /Full text and structured result/);
  assert.doesNotMatch(result.content[0].text, /RAW_SERVER_INSTRUCTIONS_PROSE/);
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
  const { textPath, jsonPath } = truncatedPaths(result.content[0].text);
  try {
    const rawSaved = readFileSync(textPath, 'utf8');
    assert.match(rawSaved, /window_id=1 pid=2 elements=1000/);
    const parsed = JSON.parse(readFileSync(jsonPath, 'utf8'));
    assert.equal(parsed.structuredContent.element_count, 1500);
    assert.equal(parsed.structuredContent.tree_markdown, 'RAW_ACCESSIBILITY_TREE_MARKDOWN_CONTENT');
    assert.equal(statSync(textPath).mode & 0o777, 0o600);
    assert.equal(statSync(jsonPath).mode & 0o777, 0o600);
    assert.equal(statSync(join(textPath, '..')).mode & 0o777, 0o700);
  } finally {
    rmSync(join(textPath, '..'), { recursive: true, force: true });
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
