// Read-only MCP stdio client: times real tool calls against one target.
// Usage: node e2e_readonly.mjs <exe> <targets.toml> <target> <pool:1|0>
import { spawn } from 'node:child_process';
import { mkdtempSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

const [exe, targets, target, pool] = process.argv.slice(2);
const state = mkdtempSync(join(tmpdir(), 'tmux-mcp-e2e-'));
// Prefer Windows OpenSSH like a natively launched client (Git's MSYS ssh hangs here).
const pathKey = Object.keys(process.env).find((key) => key.toUpperCase() === 'PATH') || 'PATH';
const env = { ...process.env, TMUX_MCP_SSH_POOL: pool, LOCALAPPDATA: state };
// E2E_GIT_SSH_FIRST=1 reproduces the hazard instead: binaries that pick ssh from PATH hang.
env[pathKey] = process.env.E2E_GIT_SSH_FIRST === '1'
  ? `C:\\Program Files\\Git\\usr\\bin;${process.env[pathKey]}`
  : `C:\\Windows\\System32\\OpenSSH;${process.env[pathKey]}`;
const child = spawn(exe, ['--targets', targets], {
  env,
  stdio: ['pipe', 'pipe', 'inherit'],
});
let buffer = '';
const pending = new Map();
child.stdout.on('data', (chunk) => {
  buffer += chunk;
  let index;
  while ((index = buffer.indexOf('\n')) >= 0) {
    const line = buffer.slice(0, index).trim();
    buffer = buffer.slice(index + 1);
    if (!line) continue;
    const message = JSON.parse(line);
    if (message.id !== undefined && pending.has(message.id)) {
      pending.get(message.id)(message);
      pending.delete(message.id);
    }
  }
});
let nextId = 1;
const send = (method, params) => new Promise((resolve) => {
  const id = nextId++;
  pending.set(id, resolve);
  child.stdin.write(JSON.stringify({ jsonrpc: '2.0', id, method, params }) + '\n');
});
const call = async (name, args) => {
  const started = performance.now();
  const response = await send('tools/call', { name, arguments: { target, ...args } });
  const ms = Math.round(performance.now() - started);
  const result = response.result || {};
  const text = (result.content || []).map((item) => item.text || '').join('');
  return { ms, isError: Boolean(result.isError || response.error), text, structured: result.structuredContent };
};

await send('initialize', {
  protocolVersion: '2025-06-18',
  capabilities: {},
  clientInfo: { name: 'e2e-readonly', version: '1' },
});
child.stdin.write(JSON.stringify({ jsonrpc: '2.0', method: 'notifications/initialized' }) + '\n');

const rows = [];
const state0 = await call('get-tmux-state', {});
rows.push(['get-tmux-state', state0.ms, state0.isError]);
const paneId = (JSON.stringify(state0.structured || state0.text).match(/%\d+/) || [])[0];
if (!paneId) {
  console.log('no pane found', state0.text.slice(0, 300));
  child.kill();
  process.exit(1);
}
for (let i = 0; i < 3; i++) {
  const r = await call('capture-pane', { paneId, lines: 5 });
  rows.push([`capture-pane #${i + 1}`, r.ms, r.isError, r.isError ? r.text.slice(0, 120) : '']);
}
for (let i = 0; i < 3; i++) {
  const r = await call('read-file', { paneId, path: '/etc/hostname' });
  rows.push([`read-file #${i + 1}`, r.ms, r.isError, r.isError ? r.text.slice(0, 120) : r.text.slice(0, 60)]);
}
const gpu = await call('gpu-snapshot', {});
rows.push(['gpu-snapshot', gpu.ms, gpu.isError]);
console.log(`pool=${pool} target=${target} pane=${paneId}`);
for (const row of rows) console.log(row.join('\t'));
child.kill();
