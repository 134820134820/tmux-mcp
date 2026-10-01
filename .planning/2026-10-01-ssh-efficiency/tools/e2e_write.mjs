// Modifying E2E on a throwaway tmux session. Writes only /tmp/tmux-mcp-e2e and the script cache.
// Usage: node e2e_write.mjs <exe> <targets.toml> <target>
import { spawn } from 'node:child_process';
import { mkdtempSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

const [exe, targets, target] = process.argv.slice(2);
const SESSION = 'tmux-mcp-e2e';
const DIR = '/tmp/tmux-mcp-e2e';
const pathKey = Object.keys(process.env).find((key) => key.toUpperCase() === 'PATH') || 'PATH';
const env = { ...process.env, LOCALAPPDATA: mkdtempSync(join(tmpdir(), 'tmux-mcp-e2e-')) };
env[pathKey] = `C:\\Windows\\System32\\OpenSSH;${process.env[pathKey]}`;
const child = spawn(exe, ['--targets', targets, '--full-tools', '--claude-channel'], {
  env, stdio: ['pipe', 'pipe', 'inherit'],
});

let buffer = '';
const pending = new Map();
const channel = [];
child.stdout.on('data', (chunk) => {
  buffer += chunk;
  let index;
  while ((index = buffer.indexOf('\n')) >= 0) {
    const line = buffer.slice(0, index).trim();
    buffer = buffer.slice(index + 1);
    if (!line) continue;
    const message = JSON.parse(line);
    if (message.method === 'notifications/claude/channel') channel.push(message.params);
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

await send('initialize', { protocolVersion: '2025-06-18', capabilities: {}, clientInfo: { name: 'e2e-write', version: '1' } });
child.stdin.write(JSON.stringify({ jsonrpc: '2.0', method: 'notifications/initialized' }) + '\n');

const tools = await send('tools/list', {});
check('write-file is listed', tools.result.tools.some((tool) => tool.name === 'write-file'));

const created = await call('create-session', { name: SESSION });
console.log('create-session ->', created.isError, created.text.slice(0, 300));
// Find the session (created now or left by an earlier attempt) and its pane in the topology.
const state = await call('get-tmux-state', {});
// get-tmux-state is flat: sessions, windows (session_id), panes (window_id).
const session = (state.data?.sessions || []).find((s) => s.name === SESSION);
const sessionId = session?.id;
const windowIds = (state.data?.windows || [])
  .filter((w) => (w.session_id ?? w.sessionId) === sessionId)
  .map((w) => w.id);
const paneId = (state.data?.panes || []).find((p) => windowIds.includes(p.window_id ?? p.windowId))?.id;
if (!paneId) console.log('state keys:', Object.keys(state.data || {}), JSON.stringify((state.data?.windows || []).slice(0, 2)));
check('throwaway session available', Boolean(paneId), `${sessionId} ${paneId}`);
if (!paneId) {
  console.log(JSON.stringify(state.data || state.text).slice(0, 600));
  child.kill();
  process.exit(1);
}
const scripts = [];
try {
  const startCwd = (await call('execute-command', { paneId, command: 'pwd', waitMs: 20000 })).data?.result?.output;

  // write-file
  let r = await call('write-file', { paneId, path: `${DIR}/note.txt`, content: '第一行\nsecond\n', createParents: true });
  check('write-file creates with createParents', !r.isError && r.data?.created === true, r.isError ? r.text : '');
  r = await call('write-file', { paneId, path: `${DIR}/note.txt`, content: 'clobber\n' });
  check('write-file refuses to overwrite by default', r.isError && r.text.includes('already exists'), r.text.slice(0, 100));
  r = await call('write-file', { paneId, path: `${DIR}/note.txt`, content: 'replaced ✓\n', overwrite: true });
  check('write-file overwrites when asked', !r.isError && r.data?.created === false, r.isError ? r.text : '');
  r = await call('read-file', { paneId, path: `${DIR}/note.txt` });
  check('content reads back exactly', r.data?.content === 'replaced ✓\n', JSON.stringify(r.data?.content));

  // User decision 2026-10-02: no ownership rule. Overwrite needs only directory write
  // permission, like a normal rename; the old file mode is kept.
  await call('execute-command', { paneId, command: `chmod 444 ${DIR}/note.txt`, waitMs: 20000 });
  r = await call('write-file', { paneId, path: `${DIR}/note.txt`, content: 'read-only replaced\n', overwrite: true });
  const replaced = await call('read-file', { paneId, path: `${DIR}/note.txt` });
  check('overwrite replaces a read-only file in a writable dir', !r.isError && replaced.data?.content === 'read-only replaced\n', r.text.slice(0, 120));
  r = await call('write-file', { paneId, path: '/etc/hostname', content: 'x\n', overwrite: true });
  check('unwritable directory still fails as not written', r.isError && r.text.includes('File not written'), r.text.slice(0, 120));
  // A name starting with '-' is a file, never an ln/mktemp option.
  r = await call('write-file', { paneId, path: '-tdash.txt', content: 'dash\n' });
  check('dash-leading relative name is created literally', !r.isError && r.data?.created === true, r.isError ? r.text : '');

  // script: comments, &, heredoc, cd, nonzero exit
  const script = [
    'set -u',
    `cd ${DIR}  # comments are allowed in scripts`,
    "cat > from-heredoc.txt <<'EOF'",
    'heredoc $HOME stays literal',
    'EOF',
    '(sleep 1; echo background-done > bg.txt) &',
    'wait',
    'echo "cwd=$(pwd)"; cat from-heredoc.txt bg.txt',
    'exit 3',
  ].join('\n');
  r = await call('execute-command', { paneId, script, waitMs: 30000 });
  if (r.data?.scriptPath) scripts.push(r.data.scriptPath);
  const res = r.data?.result;
  check('script ran tracked with exit code 3', res?.status === 'failed' && res?.exitCode === 3, r.isError ? r.text.slice(0, 160) : `${res?.status} ${res?.exitCode}`);
  check('script output complete', (res?.output || '').includes('heredoc $HOME stays literal') && res.output.includes('background-done') && res.output.includes(`cwd=${DIR}`), JSON.stringify(res?.output));

  r = await call('execute-command', { paneId, command: 'pwd', waitMs: 20000 });
  check('pane shell alive; script cd did not leak', r.data?.result?.status === 'completed' && r.data.result.output === startCwd, `${r.data?.result?.output} vs ${startCwd}`);

  r = await call('execute-command', { paneId, script: 'echo before\nexit 0\n', waitMs: 20000 });
  if (r.data?.scriptPath) scripts.push(r.data.scriptPath);
  const after = await call('execute-command', { paneId, command: 'echo still-here', waitMs: 20000 });
  check('exit in script does not close the pane', after.data?.result?.output === 'still-here', after.isError ? after.text : after.data?.result?.output);

  const capturedBefore = (await call('capture-pane', { paneId, lines: 200 })).text;
  r = await call('execute-command', { paneId, script: 'if true; then\n  echo "unterminated\nfi\n' });
  const capturedAfter = (await call('capture-pane', { paneId, lines: 200 })).text;
  check('syntax error rejected before typing', r.isError && r.text.includes('bash -n') && capturedBefore === capturedAfter, r.text.slice(0, 140));

  // paste-text verifies the staged buffer byte-for-byte before typing (real tmux round trip).
  // Bracketed paste leaves the text on the input line until Enter is pressed.
  r = await call('paste-text', { paneId, content: "printf 'paste-%s\\n' 'ok-中文✓'" });
  await sleep(1000);
  const staged = await call('capture-pane', { paneId, lines: 20 });
  check('paste-text delivers exact bytes on real tmux', !r.isError && staged.text.includes("printf 'paste-%s\\n' 'ok-中文✓'"), r.isError ? r.text.slice(0, 160) : staged.text.slice(-200));
  // Unsubmitted-input guard: neither a command nor a second paste may be appended to it.
  r = await call('execute-command', { paneId, command: 'echo must-not-append', waitMs: 3000 });
  const p2 = await call('paste-text', { paneId, content: 'echo second-paste' });
  await sleep(500);
  const untouched = await call('capture-pane', { paneId, lines: 20 });
  check('execute-command and second paste refused while paste is unsubmitted',
    r.isError && r.text.includes('unsubmitted input') && p2.isError && !untouched.text.includes('must-not-append') && !untouched.text.includes('second-paste'),
    `${r.text.slice(0, 100)} | ${p2.text.slice(0, 60)}`);
  await call('press-special-key', { paneId, key: 'enter' });
  await sleep(1000);
  const ran = await call('capture-pane', { paneId, lines: 20 });
  check('pasted line runs after Enter', ran.text.includes('paste-ok-中文✓'), ran.text.slice(-200));

  // notify
  r = await call('execute-command', { paneId, command: 'sleep 4; echo notified', notify: true });
  const commandId = r.data?.commandId;
  for (let i = 0; i < 40 && !channel.some((c) => c.meta?.command_id === commandId); i++) await sleep(500);
  const note = channel.find((c) => c.meta?.command_id === commandId);
  check('completion announced on the channel', Boolean(note) && note.content.includes('completed'), note?.content);
} finally {
  // The pane cwd is the session start dir (scripts' cd does not leak), where -tdash.txt landed.
  const base = `rm -rf ${DIR}; rm -f -- ./-tdash.txt`;
  const cleanup = scripts.length ? `${base}; rm -f ${scripts.map((p) => `'${p}'`).join(' ')}` : base;
  const cleaned = await call('execute-command', { paneId, command: cleanup, waitMs: 20000 });
  check('cleanup of test dir and uploaded scripts', cleaned.data?.result?.status === 'completed', cleaned.isError ? cleaned.text : '');
  const killed = await call('kill-target', { targetId: sessionId || SESSION });
  check('throwaway session removed', !killed.isError, killed.isError ? killed.text.slice(0, 160) : sessionId);
  console.log(`\n${failures ? `${failures} FAILED` : 'ALL PASSED'}`);
  child.kill();
}
