import test from 'node:test';
import assert from 'node:assert/strict';
import { CodexProxy, quotaError, authIdentity, CONTINUE } from './codex-proxy.mjs';
const error = { message: 'Usage limit reached', codexErrorInfo: 'usageLimitExceeded' };
function harness(recover = async () => ({ ok: true, identity: 'b:personal', auth: { type: 'chatgptAuthTokens', accessToken: 'TEST-ACCESS', chatgptAccountId: 'personal' } }), initialAuth = null) {
  const native = [], client = [], recoveries = [];
  const proxy = new CodexProxy({ identity: 'a:personal', initialAuth,
    toClient: m => client.push(structuredClone(m)),
    toNative: m => {
      native.push(structuredClone(m));
      if (m.method === 'account/login/start') queueMicrotask(() => proxy.native({ id: m.id, result: { type: 'chatgptAuthTokens' } }));
    }, recover: request => { recoveries.push(request); return recover(request); },
  });
  function start(thread = 'thread', turn = 'turn') {
    proxy.client({ id: `start-${turn}`, method: 'turn/start', params: { threadId: thread, model: 'same-model', input: [{ type: 'text', text: 'original task' }] } });
    proxy.native({ method: 'turn/started', params: { threadId: thread, turn: { id: turn, status: 'inProgress' } } });
  }
  function fail(thread = 'thread', turn = 'turn', err = error) {
    proxy.native({ method: 'error', params: { threadId: thread, turnId: turn, willRetry: false, error: err } });
    proxy.native({ method: 'turn/completed', params: { threadId: thread, turn: { id: turn, status: 'failed', error: err } } });
  }
  return { proxy, native, client, recoveries, start, fail };
}
test('only structured quota failures trigger account recovery', () => {
  assert.ok(quotaError(error)); assert.ok(quotaError({ codexErrorInfo: { responseTooManyFailedAttempts: { httpStatusCode: 429 } } }));
  for (const info of ['unauthorized', 'serverOverloaded', 'contextWindowExceeded']) assert.equal(quotaError({ codexErrorInfo: info }), false);
  assert.equal(quotaError({ message: 'quota reached' }), false);
});
test('ordinary requests, approvals and notifications are forwarded unchanged', () => {
  const h = harness(); const request = { id: 42, method: 'item/commandExecution/requestApproval', params: { turnId: 'native' } };
  h.proxy.native(request); assert.deepEqual(h.client[0], request);
  h.proxy.client({ id: 42, result: { decision: 'decline' } }); assert.deepEqual(h.native[0], { id: 42, result: { decision: 'decline' } }); h.proxy.close();
});
test('initial account uses external access tokens before forwarding account or turn requests', async () => {
  const h = harness(undefined, { type: 'chatgptAuthTokens', accessToken: 'TEST-ACCESS', chatgptAccountId: 'personal' });
  h.proxy.client({ method: 'initialized' });
  h.proxy.client({ id: 3, method: 'account/read', params: {} });
  assert.deepEqual(h.native.map(m => m.method), ['initialized', 'account/login/start']);
  await h.proxy.authReady; await new Promise(r => setImmediate(r));
  assert.equal(h.native.at(-1).method, 'account/read'); assert.equal(h.proxy.externalAuth, true);
  assert.equal(h.client.some(m => JSON.stringify(m).includes('TEST-ACCESS')), false); h.proxy.close();
});
test('external refresh returns access tokens only to native Codex and preserves identity', async () => {
  const h = harness(async request => ({ ok: true, identity: request.identity, auth: { accessToken: 'REFRESHED', chatgptAccountId: 'personal' } }));
  h.proxy.externalAuth = true;
  await h.proxy.refresh({ id: 'refresh' });
  assert.deepEqual(h.recoveries[0], { excludedIdentities: [], refresh: true, identity: 'a:personal' });
  assert.deepEqual(h.native.at(-1), { id: 'refresh', result: { accessToken: 'REFRESHED', chatgptAccountId: 'personal', chatgptPlanType: null } });
  assert.deepEqual(h.client, []); h.proxy.close();
});
test('external refresh refuses a changed identity', async () => {
  const h = harness(); await h.proxy.refresh({ id: 'refresh' });
  assert.equal(h.native.at(-1).error.code, -32000); assert.deepEqual(h.client, []); h.proxy.close();
});
test('Piaz manual switch updates the running native session and retains task continuation', async () => {
  const h = harness(); h.start(); h.proxy.externalAuth = true;
  h.proxy.readLogin = async () => ({ identity: 'b:personal', auth: { type: 'chatgptAuthTokens', accessToken: 'TEST-B', chatgptAccountId: 'personal' } });
  await Promise.all([h.proxy.sync(), h.proxy.sync()]);
  assert.equal(h.proxy.identity, 'b:personal');
  assert.equal(h.native.filter(m => m.method === 'account/login/start').length, 1);
  assert.equal(h.native.some(m => m.method === 'turn/interrupt'), false);
  // An old in-flight request can fail after the new login was acknowledged.
  h.fail(); await h.proxy.queue;
  assert.equal(h.recoveries.length, 0);
  assert.equal(h.client.at(-1).params.turn.error.codexErrorInfo, 'serverOverloaded');
  h.proxy.close();
});
test('next turn waits for Piaz authentication sync before native turn/start', async () => {
  const h = harness(); h.proxy.externalAuth = true;
  h.proxy.readLogin = async () => ({ identity: 'b:personal', auth: { type: 'chatgptAuthTokens', accessToken: 'TEST-B', chatgptAccountId: 'personal' } });
  h.proxy.client({ id: 'next', method: 'turn/start', params: { threadId: 'thread', input: [] } });
  assert.equal(h.native.length, 0);
  await h.proxy.queue; await new Promise(r => setImmediate(r));
  assert.deepEqual(h.native.map(m => m.method), ['account/login/start', 'turn/start']);
  assert.equal(h.proxy.starts.get('thread').identity, 'b:personal'); h.proxy.close();
});
test('network context revoked by a successful token handoff continues without selecting another account', async () => {
  const h = harness(); h.start(); h.proxy.externalAuth = true;
  h.proxy.readLogin = async () => ({ identity: 'a:personal', auth: { type: 'chatgptAuthTokens', accessToken: 'NEW-A', chatgptAccountId: 'personal' } });
  await h.proxy.sync();
  h.fail('thread', 'turn', { message: 'Fatal error: application network permission was revoked', codexErrorInfo: 'other' });
  await h.proxy.queue;
  assert.equal(h.recoveries.length, 0);
  assert.equal(h.client.at(-1).params.turn.error.codexErrorInfo, 'serverOverloaded');
  assert.equal(h.proxy.continuations.size, 1); h.proxy.close();
});
test('failed manual authentication sync does not silently start on the old account', async () => {
  const h = harness(); h.proxy.externalAuth = true;
  h.proxy.readLogin = async () => ({ identity: 'b:personal', auth: { accessToken: 'TEST-B' } });
  h.proxy.request = async () => { throw new Error('refused'); };
  h.proxy.client({ id: 'next', method: 'turn/start', params: { threadId: 'thread', input: [] } });
  await h.proxy.queue; await new Promise(r => setImmediate(r));
  assert.equal(h.native.some(m => m.method === 'turn/start'), false);
  assert.equal(h.client.at(-1).id, 'next'); assert.ok(h.client.at(-1).error); h.proxy.close();
});
test('quota rotation preserves actual turn ids and uses T3 continuation', async () => {
  const h = harness(); h.start(); h.fail(); await h.proxy.queue;
  assert.equal(h.recoveries.length, 1); assert.deepEqual(h.recoveries[0].excludedIdentities, ['a:personal']);
  assert.equal(h.client.at(-1).params.turn.id, 'turn'); assert.equal(h.client.at(-1).params.turn.error.codexErrorInfo, 'serverOverloaded');
  assert.equal(h.client.some(m => JSON.stringify(m).includes('TEST-ACCESS')), false);
  h.start('thread', 'real-next-id');
  assert.equal(h.native.at(-1).params.input[0].text, CONTINUE); assert.equal(h.native.at(-1).params.model, 'same-model');
  h.proxy.native({ method: 'turn/completed', params: { threadId: 'thread', turn: { id: 'real-next-id', status: 'completed' } } });
  assert.equal(h.client.at(-1).params.turn.id, 'real-next-id'); h.proxy.close();
});
test('unavailable quota returns the original failure without pretending success', async () => {
  const h = harness(async () => { throw new Error('all accounts exhausted'); }); h.start(); h.fail(); await h.proxy.queue;
  assert.deepEqual(h.client.at(-1).params.turn.error, error); assert.equal(h.proxy.continuations.size, 0); h.proxy.close();
});
test('401 and context errors do not switch accounts', async () => {
  for (const code of ['unauthorized', 'contextWindowExceeded']) { const h = harness(); h.start(); h.fail('thread', 'turn', { codexErrorInfo: code }); await h.proxy.queue; assert.equal(h.recoveries.length, 0); h.proxy.close(); }
});
test('simultaneous failures share one account rotation', async () => {
  const h = harness(); h.start('one', 't1'); h.start('two', 't2'); h.fail('one', 't1'); h.fail('two', 't2'); await h.proxy.queue;
  assert.equal(h.recoveries.length, 1); assert.equal(h.proxy.continuations.size, 2); h.proxy.close();
});
test('Stop during recovery prevents login and continuation', async () => {
  let resolve; const h = harness(() => new Promise(r => { resolve = r; })); h.start(); h.fail(); await new Promise(r => setImmediate(r));
  h.proxy.client({ id: 'stop', method: 'turn/interrupt', params: { threadId: 'thread', turnId: 'turn' } });
  resolve({ ok: true, identity: 'b:personal', auth: {} }); await h.proxy.queue;
  assert.equal(h.proxy.continuations.size, 0); assert.equal(h.native.some(m => m.method === 'account/login/start'), false);
  assert.equal(h.client.at(-1).params.turn.status, 'interrupted'); h.proxy.close();
});
test('native subagents are passed through instead of being silently retried', async () => {
  const h = harness(); h.proxy.native({ method: 'turn/started', params: { threadId: 'native-child', turn: { id: 'child' } } });
  h.fail('native-child', 'child'); await h.proxy.queue; assert.equal(h.recoveries.length, 0); assert.equal(h.client.at(-1).params.turn.error.codexErrorInfo, 'usageLimitExceeded'); h.proxy.close();
});
test('identity matches Piaz user/workspace scheme', () => {
  const claims = Buffer.from(JSON.stringify({ sub: 'fallback', 'https://api.openai.com/auth': { chatgpt_user_id: 'user', chatgpt_account_id: 'workspace' } })).toString('base64url');
  assert.equal(authIdentity({ tokens: { id_token: `header.${claims}.sig` } }), 'user:workspace'); assert.equal(authIdentity({}), null);
});
