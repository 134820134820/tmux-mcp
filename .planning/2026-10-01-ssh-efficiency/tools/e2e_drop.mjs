// Real-server network-drop E2E on a throwaway tmux session (no files written).
// Kills the local ssh process of the MCP's `tmux wait-for` watcher while a command runs, then
// checks that tracking still finishes the command (polling path) and the pane is reusable.
// Usage: node e2e_drop.mjs <exe> <targets.toml> <target>
import { spawn, execFileSync } from 'node:child_process';
import { mkdtempSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

const [exe, targets, target] = process.argv.slice(2);
const SESSION = 'tmux-mcp-e2e-drop';
const pathKey = Object.keys(process.env).find((key) => key.toUpperCase() === 'PATH') || 'PATH';
const env = { ...process.env, LOCALAPPDATA: mkdtempSync(join(tmpdir(), 'tmux-mcp-e2e-')) };
env[pathKey] = `C:\\Windows\\System32\\OpenSSH;${process.env[pathKey]}`;
const child = spawn(exe, ['--targets', targets, '--full-tools'], { env, stdio: ['pipe', 'pipe', 'inherit'] });

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
  const response = await send('tools/call', { name, arguments: { target, ...args } });
  const result = response.result || {};
  const text = (result.content || []).map((item) => item.text || '').join('');
  return { isError: Boolean(result.isError || response.error), text: text || JSON.stringify(response.error), data: result.structuredContent };
};
let failures = 0;
const check = (label, ok, detail = '') => {
  if (!ok) failures++;
  console.log(`${ok ? 'PASS' : 'FAIL'}  ${label}${detail ? `  -- ${detail}` : ''}`);
};
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

// Kill only this MCP's own ssh children whose command line runs `tmux wait-for`.
const killWatchers = () => {
  const script = `Get-CimInstance Win32_Process -Filter "ParentProcessId=${child.pid}" | ` +
    `Where-Object { $_.CommandLine -like '*wait-for*' } | ` +
    `ForEach-Object { Stop-Process -Id $_.ProcessId -Force; $_.ProcessId }`;
  const out = execFileSync('pwsh', ['-NoProfile', '-Command', script], { encoding: 'utf8' });
  return out.split(/\s+/).filter(Boolean);
};
const waitTerminal = async (commandId) => {
  let snapshot;
  for (let i = 0; i < 6; i++) {
    const r = await call('get-command-result', { commandId, waitMs: 30000 });
    snapshot = r.data?.result ?? r.data;
    if (snapshot && snapshot.status !== 'running' && snapshot.status !== 'pending') return { snapshot, text: r.text };
  }
  return { snapshot, text: 'still running after 180 s' };
};

await send('initialize', { protocolVersion: '2025-06-18', capabilities: {}, clientInfo: { name: 'e2e-drop', version: '1' } });
child.stdin.write(JSON.stringify({ jsonrpc: '2.0', method: 'notifications/initialized' }) + '\n');

await call('create-session', { name: SESSION });
const state = await call('get-tmux-state', {});
const session = (state.data?.sessions || []).find((s) => s.name === SESSION);
const sessionId = session?.id;
const windowIds = (state.data?.windows || []).filter((w) => (w.session_id ?? w.sessionId) === sessionId).map((w) => w.id);
const paneId = (state.data?.panes || []).find((p) => windowIds.includes(p.window_id ?? p.windowId))?.id;
check('throwaway session available', Boolean(paneId), `${sessionId} ${paneId}`);
if (!paneId) { child.kill(); process.exit(1); }

const dropCase = async (label, command, expectOutput) => {
  const started = await call('execute-command', { paneId, command, waitMs: 1500 });
  const commandId = started.data?.commandId;
  check(`${label}: started and still running`, Boolean(commandId) && started.data?.status === 'running', started.text.slice(0, 160));
  let killed = [];
  for (let i = 0; i < 10 && killed.length === 0; i++) { killed = killWatchers(); if (!killed.length) await sleep(500); }
  check(`${label}: watcher ssh killed (simulated drop)`, killed.length > 0, `pids ${killed.join(',')}`);
  const t0 = Date.now();
  const { snapshot, text } = await waitTerminal(commandId);
  const secs = ((Date.now() - t0) / 1000).toFixed(1);
  check(`${label}: finished as completed exit 0 (${secs} s after drop)`,
    snapshot?.status === 'completed' && snapshot?.exitCode === 0, JSON.stringify(snapshot ?? text).slice(0, 220));
  if (expectOutput) check(`${label}: output intact`, (snapshot?.output || '').includes(expectOutput), JSON.stringify(snapshot?.output).slice(0, 120));
};

try {
  // START stays visible: normal polling path.
  await dropCase('short output', 'sleep 8; echo drop-ok', 'drop-ok');
  // 5000 lines push START beyond tmux history (default 2000): the case that used to hang.
  await dropCase('START scrolled out', 'sleep 8; seq 1 5000', null);
  const after = await call('execute-command', { paneId, command: 'echo pane-reusable', waitMs: 20000 });
  check('pane released and reusable', after.data?.result?.status === 'completed' && (after.data.result.output || '').includes('pane-reusable'), after.text.slice(0, 160));
} finally {
  const killed = await call('kill-target', { targetId: sessionId || SESSION });
  check('throwaway session removed', !killed.isError, killed.isError ? killed.text.slice(0, 160) : sessionId);
  console.log(`\n${failures ? `${failures} FAILED` : 'ALL PASSED'}`);
  child.kill();
}
