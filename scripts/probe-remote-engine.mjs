// Read-only live SSH/RPC feasibility probe. Requires Node 22+ and working SSH.
// Usage: node scripts/probe-remote-engine.mjs kidsseeghosts 27656
// Does not start engines, submit prompts, or change remote workspace data.
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { once } from 'node:events';
import net from 'node:net';
import { setTimeout as delay } from 'node:timers/promises';

const [host, portText] = process.argv.slice(2);
const remotePort = Number(portText);
assert(host && !host.startsWith('-'), 'Provide an SSH host alias');
assert(Number.isInteger(remotePort) && remotePort > 0 && remotePort < 65536,
  'Provide the remote engine IPC port');
const reservation = net.createServer();
reservation.listen(0, '127.0.0.1');
await once(reservation, 'listening');
const localPort = reservation.address().port;
await new Promise(resolve => reservation.close(resolve));
const url = `ws://127.0.0.1:${localPort}`;
let tunnel;
let tunnelError = '';
const clients = [];

function startTunnel() {
  tunnelError = '';
  tunnel = spawn('ssh', ['-N', '-o', 'BatchMode=yes', '-o', 'ConnectTimeout=8',
    '-o', 'ExitOnForwardFailure=yes', '-o', 'ServerAliveInterval=5',
    '-o', 'ServerAliveCountMax=2', '-L',
    `127.0.0.1:${localPort}:127.0.0.1:${remotePort}`, host],
  { stdio: ['ignore', 'ignore', 'pipe'] });
  tunnel.stderr.on('data', data => { tunnelError += data; });
  tunnel.on('error', error => { tunnelError += error.message; });
}

async function stopTunnel() {
  if (!tunnel?.pid || tunnel.exitCode !== null || tunnel.signalCode !== null) return;
  const exited = once(tunnel, 'exit');
  tunnel.kill('SIGTERM');
  await exited;
}

async function connect() {
  const socket = new WebSocket(url);
  await new Promise((resolve, reject) => {
    const timer = setTimeout(() => { socket.close(); reject(new Error('WS timeout')); }, 3000);
    socket.addEventListener('open', () => { clearTimeout(timer); resolve(); }, { once: true });
    socket.addEventListener('error', () => { clearTimeout(timer); reject(new Error('WS dial failed')); }, { once: true });
  });
  let next = 0;
  const pending = new Map();
  socket.addEventListener('message', event => {
    for (const line of String(event.data).split('\n').filter(Boolean)) {
      const frame = JSON.parse(line);
      const waiter = pending.get(frame.id);
      if (!waiter) continue;
      if (frame.err) waiter.reject(new Error(frame.err));
      else if (Object.hasOwn(frame, 'item') || Object.hasOwn(frame, 'ok')) {
        waiter.resolve(Object.hasOwn(frame, 'item') ? frame.item : frame.ok);
        if (Object.hasOwn(frame, 'item')) socket.send(JSON.stringify({ id: frame.id, cancel: true }));
      } else continue;
      clearTimeout(waiter.timer);
      pending.delete(frame.id);
    }
  });
  socket.addEventListener('close', () => {
    for (const waiter of pending.values()) {
      clearTimeout(waiter.timer);
      waiter.reject(new Error('connection closed'));
    }
    pending.clear();
  });
  const client = { socket, call(method, params = {}) {
    return new Promise((resolve, reject) => {
      const id = ++next;
      const timer = setTimeout(() => { pending.delete(id); reject(new Error(`${method} timeout`)); }, 8000);
      pending.set(id, { resolve, reject, timer });
      socket.send(JSON.stringify({ id, method, params }));
    });
  } };
  clients.push(client);
  return client;
}

async function waitForClient() {
  const end = Date.now() + 12000;
  while (Date.now() < end) {
    if (!tunnel.pid || tunnel.exitCode !== null) throw new Error(`SSH exited: ${tunnelError}`);
    try { return await connect(); } catch { await delay(100); }
  }
  throw new Error(`Tunnel unavailable: ${tunnelError}`);
}

try {
  startTunnel();
  const a = await waitForClient();
  const identity = await a.call('EngineInfo');
  assert(identity.deviceId && identity.workspaceScope);
  await a.call('EngineReady');
  const summary = { host, remotePort, workspaceScope: identity.workspaceScope, checks: [] };
  let chats = [];
  for (const method of ['WatchDevices', 'WatchChats', 'WatchSpaces', 'ListRepos']) {
    const rows = await a.call(method);
    assert(Array.isArray(rows), `${method} should return an array`);
    summary.checks.push({ method, rows: rows.length });
    if (method === 'WatchChats') chats = rows;
  }
  // Read only an existing transcript and a conventional README; never print contents.
  if (chats.length) {
    const chatId = chats[0].id;
    assert(chatId, 'Chat row should have an id');
    const transcript = await a.call('WatchDocMessages', { chatId });
    assert(transcript && typeof transcript === 'object');
    summary.transcriptRead = 'passed';
    const listing = await a.call('ListWorkspaceDirectory', { chatId });
    assert(Array.isArray(listing.entries));
    summary.workspaceDirectoryRead = 'passed';
    const readme = listing.entries.find(entry => entry.kind === 'file'
      && /^readme(?:\.md|\.txt)?$/i.test(entry.name));
    if (readme) {
      const file = await a.call('ReadWorkspaceFile', { chatId, path: readme.path });
      assert.equal(typeof file.text, 'string');
      assert(file.contentHash);
      summary.workspaceReadmeRead = 'passed';
    } else summary.workspaceReadmeRead = 'skipped: no README in root listing';
  } else summary.transcriptRead = 'skipped: no existing chats';
  const b = await connect();
  assert.deepEqual(await b.call('EngineInfo'), identity);
  summary.concurrentClients = 'passed';
  const times = [];
  for (let i = 0; i < 30; i++) {
    const start = performance.now();
    assert.deepEqual(await a.call('EngineInfo'), identity);
    times.push(performance.now() - start);
  }
  times.sort((a, b) => a - b);
  summary.rpcLatencyMs = { samples: times.length,
    median: Number(times[15].toFixed(2)), p95: Number(times[28].toFixed(2)) };
  const closed = Promise.all(clients.map(({ socket }) => new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error('socket did not close after SSH exit')), 5000);
    socket.addEventListener('close', () => { clearTimeout(timer); resolve(); }, { once: true });
  })));
  await stopTunnel();
  await closed;
  summary.disconnectDetected = 'passed';
  startTunnel();
  const c = await waitForClient();
  assert.deepEqual(await c.call('EngineInfo'), identity);
  assert(Array.isArray(await c.call('WatchChats')));
  summary.explicitReconnectAndResubscribe = 'passed';
  console.log(JSON.stringify(summary, null, 2));
} finally {
  for (const { socket } of clients) socket.close();
  await stopTunnel();
}
