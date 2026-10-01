// Run: node tests/web_target_switch.mjs
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import vm from 'node:vm';

const page = readFileSync(new URL('../web/index.html', import.meta.url), 'utf8');
const source = (start, end) => page.slice(page.indexOf(start), page.indexOf(end, page.indexOf(start)));
const select = { options: [], value: '', appendChild(option) { this.options.push(option); } };
const requests = [];
const errors = [];
let clears = 0;
const context = vm.createContext({
  URLSearchParams, encodeURIComponent, Promise,
  selectedTarget: 'a', selectedPane: '%0', latestState: null,
  stateBusy: false, stateRefreshQueued: false, captureBusy: false,
  mode: 'interactive', keyQueue: Promise.resolve(), decidedApprovalIds: new Set(),
  elements: { targetSelect: select, terminal: { textContent: '', scrollHeight: 0 } },
  localStorage: { setItem() {} },
  node: (tag, cls, text) => ({ textContent: text }),
  clear: (element) => { clears++; element.options = []; },
  api: (path, options) => new Promise((resolve, reject) => requests.push({ path, options, resolve, reject })),
  flattenPanes: () => [], choosePane: () => false,
  updateActivity() {}, renderTopology() {}, renderAiPause() {}, renderGateMode() {},
  renderOperations() {}, renderApproval() {}, renderMessages() {}, setConnection() {}, renderTrackingErrors() {},
  showError: (message) => { if (message) errors.push(message); },
  setTimeout: () => { throw new Error('Unexpected scheduled refresh'); }
});
vm.runInContext([
  source('function renderTargets(', 'function renderGateMode('),
  source('async function refreshState()', 'async function refreshCapture()'),
  source('async function refreshCapture()', 'function sendKey('),
  source('function sendKey(', 'function setMode(')
].join('\n'), context);

// A slow response for A must not revert the user's switch to B; B must still be fetched.
const oldRefresh = context.refreshState();
context.selectedTarget = 'b';
context.refreshState();
requests.shift().resolve({ target: 'a', targets: { a: {}, b: {} } });
await oldRefresh;
assert.equal(context.selectedTarget, 'b', 'stale state response reverted target selection');
assert.equal(context.latestState, null, 'stale state was rendered');
assert.match(requests[0].path, /target=b/);
requests.shift().resolve({ target: 'b', targets: { a: {}, b: {} } });
await new Promise(setImmediate);
assert.equal(context.latestState.target, 'b');
assert.equal(context.stateBusy, false);

// Normal polling must preserve existing option nodes, so an open picker is not reset.
const firstOption = select.options[0];
const before = clears;
context.renderTargets({ a: {}, b: {} }, 'b');
assert.equal(clears, before, 'polling rebuilt unchanged target options');
assert.equal(select.options[0], firstOption);
context.renderTargets({ a: { note: 'updated' }, b: {} }, 'b');
assert.match(select.options[0].textContent, /updated/);

// Errors belonging to a previous target must not mark the current target disconnected.
const staleError = context.refreshState();
context.selectedTarget = 'a';
requests.shift().reject(new Error('old target error'));
await staleError;
assert.deepEqual(errors, []);

// Different targets can both own %0: neither stale terminal output nor errors may leak.
for (const fail of [false, true]) {
  context.selectedTarget = 'a';
  const capture = context.refreshCapture();
  const request = requests.shift();
  assert.match(request.path, /target=a/);
  context.selectedTarget = 'b';
  if (fail) request.reject(new Error('old capture error'));
  else request.resolve({ text: 'secret from target a' });
  await capture;
  assert.equal(context.elements.terminal.textContent, '');
  assert.deepEqual(errors, []);
}

// Queued input keeps the target selected when the key was pressed.
context.selectedTarget = 'a';
context.sendKey('x', true);
context.selectedTarget = 'b';
await Promise.resolve();
assert.equal(requests[0].options.body.target, 'a', 'queued key sent to a different target');
requests.shift().resolve({ ok: true });
await context.keyQueue;
console.log('PASS: target switching, polling, stale state/capture, queued input');

// Execute the actual pause UI and its handlers, including the recovery handshake.
let focused = null;
class Element {
  constructor(tag, text = '') { this.tag = tag; this._text = text; this.children = []; this.dataset = {}; this.handlers = {}; }
  get textContent() { return this._text + this.children.map(child => child.textContent).join(''); }
  set textContent(text) { this._text = text; this.children = []; }
  appendChild(child) { this.children.push(child); }
  addEventListener(type, handler) { this.handlers[type] = handler; }
  focus() { focused = this; }
}
const buttonsOf = element => [
  ...(element.tag === 'button' ? [element] : []),
  ...element.children.flatMap(buttonsOf)
];
const banner = new Element('section');
const panel = { hidden: true, open: false, dataset: {} };
const title = { textContent: '' };
const meta = { textContent: '' };
const uiRequests = [];
const uiErrors = [];
let inspected = '';
let shownMode = '';
const safety = vm.createContext({
  elements: { trackingErrors: banner, safetyPanel: panel, safetyTitle: title, safetyMeta: meta }, Date, JSON,
  selectedTarget: 'seven-intern', currentAiPause: null, trackingNotices: [], confirmingPauseId: '',
  focusPauseId: '', resolvedPausesOpen: false, surfacedPauses: new Set(),
  node: (tag, cls, text) => new Element(tag, text), clear: element => { element.textContent = ''; },
  confirm: () => { throw new Error('native confirm dialogs are replaced by inline confirmation'); },
  selectPane: pane => { inspected = pane; }, setMode: mode => { shownMode = mode; },
  api: async (path, options) => { uiRequests.push({ path, options }); },
  refreshState: async () => {}, showError: error => uiErrors.push(error)
});
vm.runInContext(source('function renderTrackingErrors(', 'function flattenPanes('), safety);
const notice = { target: 'seven-intern', source: 'Claude Code', paneId: '%5', commandId: 'uncertain',
  reason: '<img src=x onerror=alert(1)>', updatedAtMs: 1000, recovery: 'paused' };
safety.renderTrackingErrors([notice]);
assert.equal(banner.hidden, false);
assert.equal(panel.hidden, false);
assert.equal(panel.open, true, 'a new pause must expand the panel');
assert.equal(panel.dataset.tone, 'alert');
assert.equal(title.textContent, 'AI 操作已暂停');
for (const expected of ['Claude Code', '%5', 'uncertain', '<img src=x onerror=alert(1)>']) {
  assert.ok(banner.textContent.includes(expected), `missing ${expected}`);
}
const card = banner.children[0];
safety.renderTrackingErrors([notice]);
assert.equal(banner.children[0], card, 'polling must preserve focused action buttons');
panel.open = false;
safety.renderTrackingErrors([notice]);
assert.equal(panel.open, false, 'polling must respect a panel the user collapsed');
const [inspect, resume] = buttonsOf(card);
inspect.handlers.click();
assert.equal(inspected, '%5');
assert.equal(shownMode, 'interactive');

// The first click only asks for confirmation inline; cancelling submits nothing.
resume.handlers.click();
const confirmCard = banner.children[0];
assert.equal(confirmCard.dataset.confirming, '');
assert.ok(confirmCard.textContent.includes('确认已检查 seven-intern 的 pane %5'));
const [cancel, firstConfirm] = buttonsOf(confirmCard);
assert.equal(focused, firstConfirm, 'confirmation moves focus to the confirm button');
cancel.handlers.click();
assert.equal(uiRequests.length, 0, 'cancelled confirmation must not submit');
assert.equal(banner.children[0].dataset.confirming, undefined);
assert.equal(focused, buttonsOf(banner.children[0])[1], 'cancel returns focus to the resume button');

buttonsOf(banner.children[0])[1].handlers.click();
const confirmButton = buttonsOf(banner.children[0])[1];
safety.selectedTarget = 'eight-intern';
await confirmButton.handlers.click();
assert.equal(uiRequests.length, 0, 'stale account button must not submit');
safety.selectedTarget = 'seven-intern';
await confirmButton.handlers.click();
assert.equal(uiRequests[0].path, '/api/ai-pause/resume-tracking');
assert.equal(uiRequests[0].options.body.target, 'seven-intern');
assert.equal(uiRequests[0].options.body.commandId, 'uncertain');
assert.equal(uiRequests[0].options.body.confirmed, true);
assert.ok(!banner.textContent.includes('本条暂停已解除'), 'HTTP acceptance is not successful recovery');

safety.renderTrackingErrors([{ ...notice, recovery: 'requested' }]);
assert.ok(banner.textContent.includes('AI 暂停尚未解除'));
assert.equal(panel.dataset.tone, 'waiting');
assert.equal(buttonsOf(banner).length, 1, 'a pending request offers no second resume');

safety.renderTrackingErrors([{ ...notice, recovery: 'resumed' }]);
assert.ok(banner.textContent.includes('客户端已确认'));
assert.equal(panel.dataset.tone, 'quiet');
assert.equal(buttonsOf(banner).length, 1);

safety.renderTrackingErrors([{ ...notice, recovery: 'legacy' }]);
assert.equal(buttonsOf(banner).length, 1, 'legacy pauses cannot be resumed from the page');
assert.ok(banner.textContent.includes('不能通过网页热恢复'));

// Handled pauses fold away while something still needs attention.
panel.open = false;
safety.renderTrackingErrors([{ ...notice, commandId: 'fresh' }, { ...notice, recovery: 'resumed' }]);
assert.equal(panel.open, true, 'a newly surfaced pause re-expands the panel');
assert.equal(banner.children.length, 2);
assert.equal(banner.children[1].tag, 'details');
assert.ok(banner.children[1].textContent.includes('已处理 1 条'));
assert.ok(meta.textContent.includes('1 项需要处理'));

safety.renderTrackingErrors([{ ...notice, commandId: '', paneId: '未知', recovery: 'error', updatedAtMs: 0 }]);
assert.equal(buttonsOf(banner).length, 0, 'an unreadable pause has no pane to inspect');

safety.renderTrackingErrors([]);
assert.equal(banner.hidden, true);
assert.equal(panel.hidden, true);
assert.equal(banner.textContent, '');
safety.renderTrackingErrors();
assert.equal(banner.hidden, true);
new vm.Script(page.match(/<script>([\s\S]*?)<\/script>/)[1]);
console.log('PASS: pause UI inspect, inline confirmation, account isolation, pending vs confirmed, collapse, focus');
