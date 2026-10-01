// Action-log study for the "unsubmitted input" guard: what follows a paste-text or an
// Enter-less send-keys on the same pane? Usage: node paste_sequences.mjs [events.jsonl]
import { readFileSync } from 'node:fs';
import { join } from 'node:path';

const path = process.argv[2] || join(process.env.LOCALAPPDATA, 'tmux-mcp', 'events.jsonl');
const byId = new Map();
for (const line of readFileSync(path, 'utf8').split('\n')) {
  if (!line.trim()) continue;
  try { const r = JSON.parse(line); if (r.id) byId.set(r.id, r); } catch {}
}
const records = [...byId.values()].filter((r) => r.requestedAtMs).sort((a, b) => a.requestedAtMs - b.requestedAtMs);
const span = records.length ? `${new Date(records[0].requestedAtMs).toISOString()} → ${new Date(records.at(-1).requestedAtMs).toISOString()}` : '';
console.log(`records: ${records.length}  ${span}`);

const INPUT = new Set(['paste-text', 'send-keys', 'press-special-key', 'send-enter', 'send-cancel',
  'send-eof', 'send-escape', 'send-hex', 'execute-command']);
const target = (r) => r.timing?.target || r.arguments?.target || '?';
const pane = (r) => r.arguments?.paneId || r.target?.paneIds?.[0];
const leavesInput = (r) => {
  if (r.status === 'rejected' || r.status === 'failed' && r.tool !== 'execute-command') return false;
  if (r.tool === 'paste-text') return true;
  if (r.tool === 'send-keys') {
    const keys = String(r.arguments?.keys ?? '');
    return !r.arguments?.enter && !/^(Enter|C-m|C-c|Escape)$/i.test(keys.trim());
  }
  return false;
};
const label = (r) => {
  const a = r.arguments || {};
  const text = a.content ?? a.keys ?? a.key ?? '';
  const flag = r.tool === 'send-keys' ? ` enter=${Boolean(a.enter)}` : '';
  const fc = a.forCommandId ? ' (to running command)' : '';
  return `${r.tool}${flag}${fc} [${r.status}] ${JSON.stringify(String(text)).slice(0, 70)}`;
};

const panes = new Map();
for (const r of records) {
  if (!INPUT.has(r.tool) || !pane(r)) continue;
  const key = `${target(r)} ${pane(r)}`;
  if (!panes.has(key)) panes.set(key, []);
  panes.get(key).push(r);
}

const tally = {};
const examples = {};
let pasteCount = 0;
for (const [key, list] of panes) {
  for (let i = 0; i < list.length; i++) {
    const r = list[i];
    if (r.tool === 'paste-text') pasteCount++;
    if (!leavesInput(r)) continue;
    const next = list[i + 1];
    const gapS = next ? ((next.requestedAtMs - r.requestedAtMs) / 1000).toFixed(0) : '-';
    const kind = !next ? 'nothing after' : next.tool === 'paste-text' ? 'paste-text again'
      : next.tool === 'send-keys' && leavesInput(next) ? 'more send-keys (no Enter)'
      : next.tool === 'execute-command' ? 'execute-command (would concatenate)'
      : `${next.tool}${next.tool === 'send-keys' ? ` enter=${Boolean(next.arguments?.enter)}` : ''}`;
    const group = `${r.tool} → ${kind}`;
    tally[group] = (tally[group] || 0) + 1;
    (examples[group] ||= []).push(`${key} gap ${gapS}s | ${label(r)}  ⇒  ${next ? label(next) : ''}`);
  }
}
console.log(`paste-text calls: ${pasteCount}`);
for (const [group, n] of Object.entries(tally).sort((a, b) => b[1] - a[1])) {
  console.log(`\n${n}  ${group}`);
  for (const line of examples[group].slice(-4)) console.log(`   ${line}`);
}
