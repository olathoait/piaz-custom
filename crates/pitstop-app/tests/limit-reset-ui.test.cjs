const test = require('node:test');
const assert = require('node:assert/strict');
const { readFileSync } = require('node:fs');
const { join } = require('node:path');
const vm = require('node:vm');
const { randomUUID } = require('node:crypto');

const source = readFileSync(join(__dirname, '../ui/app.js'), 'utf8');
const strings = readFileSync(join(__dirname, '../ui/i18n.js'), 'utf8');
const grant = { id: 'weekly-1', label: 'Weekly reset', resetsLeft: 2, endsAt: '2099-01-01T00:00:00Z', clears: ['five_hour', 'seven_day'], usableNow: true, paused: false };

function harness(invoke) {
  const notices = [];
  const h = (tag, props, ...children) => ({ tag, props: props || {}, children: children.flat(Infinity).filter(Boolean) });
  const context = vm.createContext({
    resetCounts: new Map(), rowKey: (p, n) => `${p}|${n}`, render() {}, invoke, crypto: { randomUUID }, dialog: null, notices, h,
    renderDialog() {},
    openDialog(d) { context.dialog = d; },
    closeDialog() { context.dialog = null; },
    pushNotice(n) { notices.push(n); },
    sheetHead: (title, detail) => h('header', {}, title, detail),
    buttons: (...children) => h('footer', {}, ...children),
    keyed: (node) => node,
    failureBox: (title, error) => error && h('error', {}, title),
    button: (label, onclick) => h('button', { onclick }, label),
  });
  vm.runInContext(strings, context);
  vm.runInContext(source.slice(source.indexOf('function windowName('), source.indexOf('function duration(')), context);
  vm.runInContext(source.slice(source.indexOf('async function openLimitResets('), source.indexOf('function showMenu(')), context);
  vm.runInContext(source.slice(source.indexOf('const DIALOGS ='), source.indexOf('function openAdd(')), context);
  function nodes() {
    const all = [];
    function visit(value) {
      if (Array.isArray(value)) value.forEach(visit);
      else if (value && typeof value === 'object') { all.push(value); visit(value.children); }
    }
    visit(vm.runInContext('DIALOGS.limitResets()', context));
    return all;
  }
  return { context, notices, nodes, open: (provider = 'claude-code') => context.openLimitResets({ id: provider, name: provider === 'codex' ? 'Codex' : 'Claude Code' }, { name: 'work' }) };
}

test('checking and choosing a reset never consumes it; confirmation posts once with a stable request id', async () => {
  const calls = [];
  let finish;
  const h = harness(async (method, args) => {
    calls.push({ method, args });
    if (method === 'get_limit_resets') return { identity: 'a:org-1', eligible: true, grants: [grant] };
    return new Promise(resolve => { finish = resolve; });
  });
  await h.open();
  assert.deepEqual(calls.map(c => c.method), ['get_limit_resets']);
  h.nodes().find(n => n.tag === 'button' && n.children.includes('Reset for free')).props.onclick();
  assert.equal(calls.length, 1);
  const confirm = h.nodes().find(n => n.props.class === 'button primary');
  const pending = confirm.props.onclick();
  await confirm.props.onclick();
  assert.equal(calls.length, 2);
  assert.equal(calls[1].args.grant, 'weekly-1');
  assert.equal(calls[1].args.name, 'work');
  assert.equal(calls[1].args.identity, 'a:org-1');
  assert.equal(calls[1].args.provider, 'claude-code');
  assert.match(calls[1].args.request, /^[0-9a-f-]{36}$/);
  finish();
  await pending;
  assert.equal(h.context.dialog, null);
  assert.equal(h.notices.length, 1);
});

test('paused and unusable grants cannot be selected', async () => {
  for (const changed of [{ paused: true }, { usableNow: false }]) {
    const h = harness(async () => ({ identity: 'a:org-1', eligible: true, grants: [{ ...grant, ...changed }] }));
    await h.open();
    assert.equal(h.nodes().find(n => n.children.includes('Not usable right now')).props.disabled, true);
  }
});

test('no grants is an explicit empty state in English and Vietnamese', async () => {
  const h = harness(async () => ({ eligible: false, grants: [] }));
  await h.open();
  assert.ok(h.nodes().some(n => n.props.text === 'No usage limit resets available at this time.'));
  vm.runInContext('LANG = "vi"', h.context);
  assert.ok(h.nodes().some(n => n.props.text === 'Hiện tài khoản không có lượt reset hạn mức.'));
});

test('a failed claim reports an error and never reports success or retries automatically', async () => {
  let claims = 0;
  const h = harness(async method => {
    if (method === 'get_limit_resets') return { identity: 'a:org-1', eligible: true, grants: [grant] };
    claims++;
    throw new Error('Lost response');
  });
  await h.open();
  h.nodes().find(n => n.children.includes('Reset for free')).props.onclick();
  const request = h.context.dialog.request;
  await h.nodes().find(n => n.props.class === 'button primary').props.onclick();
  assert.equal(claims, 1);
  assert.equal(h.context.dialog.busy, false);
  assert.equal(h.context.dialog.request, request);
  assert.ok(h.nodes().some(n => n.tag === 'error'));
  assert.equal(h.notices.length, 0);
});

test('Codex shows the authoritative count and confirms a count-only reset without inventing a credit id', async () => {
  const calls = [];
  const h = harness(async (method, args) => {
    calls.push({ method, args });
    if (method === 'get_limit_resets') return { identity: 'a:personal', eligible: true, availableCount: 3,
      grants: [{ ...grant, id: '', label: '', resetsLeft: 3, endsAt: null, clears: [] }] };
  });
  await h.open('codex');
  assert.equal(calls[0].args.provider, 'codex');
  assert.ok(h.nodes().some(n => n.props.text === '3 resets left'));
  assert.ok(h.nodes().some(n => n.props.text === 'Next available reset'));
  h.nodes().find(n => n.children.includes('Reset for free')).props.onclick();
  assert.ok(h.nodes().some(n => n.props.text?.includes('one earned Codex reset')));
  assert.equal(h.nodes().some(n => n.props.text?.includes('weekly reset day stays')), false);
  await h.nodes().find(n => n.props.class === 'button primary').props.onclick();
  assert.equal(calls[1].args.provider, 'codex');
  assert.equal(calls[1].args.grant, '');
  assert.equal(calls[1].args.identity, 'a:personal');
  assert.ok(h.notices[0].lines[0].startsWith('Codex confirmed'));
});

test('Codex lost-response retry keeps both the chosen credit and idempotency key', async () => {
  const claims = [];
  const h = harness(async (method, args) => {
    if (method === 'get_limit_resets') return { identity: 'a:personal', eligible: true, availableCount: 1, grants: [grant] };
    claims.push(args);
    if (claims.length === 1) throw new Error('Lost response');
  });
  await h.open('codex');
  h.nodes().find(n => n.children.includes('Reset for free')).props.onclick();
  await h.nodes().find(n => n.props.class === 'button primary').props.onclick();
  assert.equal(h.notices.length, 0);
  await h.nodes().find(n => n.props.class === 'button primary').props.onclick();
  assert.equal(claims.length, 2);
  assert.deepEqual(claims[1], claims[0]);
  assert.equal(h.notices.length, 1);
});

test('account badges read serially, cache zero, and distinguish unavailable counts', async () => {
  const calls = [];
  let now = 100;
  const context = vm.createContext({
    Map, Promise, Number, Error, nowSecs: () => now,
    rowKey: (p, n) => `${p}|${n}`, render() {},
    h: (tag, props, label) => ({ tag, props, label }),
    t: (key, args) => args ? `${key}:${args.n}` : key,
    invoke: async (method, args) => {
      calls.push(args.name);
      if (args.name === 'error') throw Error('offline');
      if (args.provider === 'claude-code') return { eligible: args.name !== 'ineligible', grants: [{ resetsLeft: 2, paused: true }, { resetsLeft: 1 }] };
      return { availableCount: args.name === 'zero' ? 0 : 3, grants: [] };
    },
  });
  vm.runInContext('const resetCounts = new Map(); let resetReadQueue = Promise.resolve();', context);
  vm.runInContext(source.slice(source.indexOf('function resetCountBadge('), source.indexOf('function accountRow(')), context);
  const badge = name => context.resetCountBadge({ id: 'codex' }, { name });
  assert.equal(badge('zero').label, 'checkingResets');
  badge('many'); badge('error'); badge('zero');
  await vm.runInContext('resetReadQueue', context);
  assert.deepEqual(calls, ['zero', 'many', 'error']);
  assert.equal(badge('zero').label, 'resetCount:0');
  assert.equal(badge('many').label, 'resetCount:3');
  assert.equal(badge('error').label, 'resetCountUnknown');
  assert.equal(context.resetCountBadge({ id: 'cursor' }, { name: 'skip' }), null);
  const claudeBadge = name => context.resetCountBadge({ id: 'claude-code' }, { name });
  claudeBadge('claude'); claudeBadge('ineligible');
  await vm.runInContext('resetReadQueue', context);
  assert.equal(claudeBadge('claude').label, 'resetCount:3');
  assert.equal(claudeBadge('ineligible').label, 'resetCount:0');
  now += 61;
  assert.equal(badge('zero').label, 'checkingResets');
  await vm.runInContext('resetReadQueue', context);
  assert.deepEqual(calls, ['zero', 'many', 'error', 'claude', 'ineligible', 'zero']);
});
