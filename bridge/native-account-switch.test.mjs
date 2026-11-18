// Opt-in: real installed Codex, synthetic credentials, loopback model server; no paid requests.
import test from 'node:test';
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { createHash } from 'node:crypto';
import { spawn } from 'node:child_process';
import { createInterface } from 'node:readline';
import { mkdtemp, writeFile, readFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';

test('real Codex accepts a manual switch during an active turn and uses B on its next WebSocket request', { skip: !process.env.PIAZ_REAL_CODEX, timeout: 60000 }, async () => {
  const root = await mkdtemp(join(tmpdir(), 'piaz-native-switch-'));
  const sockets = new Set(), sampled = [], pending = new Map(), completed = [];
  let sequence = 0, response = 0, child, releaseFirst;
  const jwt = user => `header.${Buffer.from(JSON.stringify({ sub: user, email: `${user}@example.test`, exp: Math.floor(Date.now() / 1000) + 3600,
    'https://api.openai.com/auth': { chatgpt_user_id: user, chatgpt_account_id: user, chatgpt_plan_type: 'plus' },
    'https://api.openai.com/profile': { email: `${user}@example.test` },
  })).toString('base64url')}.signature`;
  const saveLogin = user => writeFile(join(root, 'auth.json'), JSON.stringify({ tokens: { id_token: jwt(user), access_token: jwt(user), account_id: user } }), { mode: 0o600 });
  const events = () => {
    const id = `resp_${++response}`, item = { type: 'message', id: `msg_${response}`, role: 'assistant', status: 'completed', content: [{ type: 'output_text', text: 'OK', annotations: [] }] };
    return [
      { type: 'response.created', response: { id, status: 'in_progress' } },
      { type: 'response.output_item.added', output_index: 0, item: { ...item, status: 'in_progress', content: [] } },
      { type: 'response.output_text.delta', item_id: item.id, output_index: 0, content_index: 0, delta: 'OK' },
      { type: 'response.output_item.done', output_index: 0, item },
      { type: 'response.completed', response: { id, status: 'completed', output: [item], usage: { input_tokens: 1, output_tokens: 1, total_tokens: 2 } } },
    ];
  };
  const owner = req => JSON.parse(Buffer.from(req.headers.authorization.split('.')[1], 'base64url')).sub;
  const server = createServer((req, res) => {
    req.resume();
    if (req.method === 'POST' && req.url.endsWith('/responses')) {
      sampled.push({ account: owner(req), transport: 'http' });
      const reply = () => {
        res.writeHead(200, { 'Content-Type': 'text/event-stream', Connection: 'close' });
        res.end(events().map(event => `event: ${event.type}\ndata: ${JSON.stringify(event)}\n\n`).join(''));
      };
      if (sampled.length === 1) releaseFirst = reply; else reply();
    } else {
      res.writeHead(200, { 'Content-Type': 'application/json' });
      if (req.url.endsWith('/accounts/check')) {
        const account = owner(req);
        res.end(JSON.stringify({ accounts: [{ id: account, workspace_backend_origin: 'https://chatgpt.com', account_routing_override: 'NO_CONSTRAINT' }] }));
      } else res.end('{"data":[],"models":[]}');
    }
  });
  server.on('connection', socket => { sockets.add(socket); socket.on('close', () => sockets.delete(socket)); socket.on('error', () => {}); });
  server.on('upgrade', (req, socket, head) => {
    const accept = createHash('sha1').update(req.headers['sec-websocket-key'] + '258EAFA5-E914-47DA-95CA-C5AB0DC85B11').digest('base64');
    socket.write(`HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: ${accept}\r\n\r\n`);
    let buffer = head;
    const send = value => {
      if (socket.destroyed) return;
      const data = Buffer.from(JSON.stringify(value)), header = Buffer.alloc(data.length < 126 ? 2 : 4);
      header[0] = 0x81; header[1] = data.length < 126 ? data.length : 126;
      if (data.length >= 126) header.writeUInt16BE(data.length, 2);
      socket.write(Buffer.concat([header, data]));
    };
    socket.on('data', data => {
      buffer = Buffer.concat([buffer, data]);
      while (buffer.length >= 2) {
        const opcode = buffer[0] & 15, masked = buffer[1] & 128;
        let length = buffer[1] & 127, offset = 2;
        if (length === 126) { if (buffer.length < 4) return; length = buffer.readUInt16BE(2); offset = 4; }
        if (length === 127) { if (buffer.length < 10) return; length = Number(buffer.readBigUInt64BE(2)); offset = 10; }
        if (length > 8 * 1024 * 1024) { socket.destroy(); return; }
        if (buffer.length < offset + (masked ? 4 : 0) + length) return;
        const mask = masked ? buffer.subarray(offset, offset + 4) : null; offset += masked ? 4 : 0;
        const payload = Buffer.from(buffer.subarray(offset, offset + length)); buffer = buffer.subarray(offset + length);
        if (mask) for (let i = 0; i < payload.length; i++) payload[i] ^= mask[i % 4];
        if (opcode === 8) { socket.end(); return; }
        if (![1, 2].includes(opcode)) continue;
        const request = JSON.parse(payload.toString());
        if (request.generate === false) {
          send({ type: 'response.created', response: { id: `warm_${++response}`, status: 'in_progress' } });
          send({ type: 'response.completed', response: { id: `warm_${response}`, status: 'completed', output: [] } });
        } else {
          sampled.push({ account: owner(req), transport: 'websocket' });
          const reply = () => { for (const event of events()) send(event); };
          if (sampled.length === 1) releaseFirst = reply; else reply();
        }
      }
    });
  });
  const waitFor = async condition => {
    for (let i = 0; i < 400; i++) { if (await condition()) return; await new Promise(resolve => setTimeout(resolve, 100)); }
    throw new Error('Native account-switch check timed out');
  };
  try {
    await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
    await saveLogin('account-a');
    await writeFile(join(root, 'config.toml'), `model="gpt-5.6-sol"\nmodel_provider="mock"\ncli_auth_credentials_store="file"\nchatgpt_base_url="http://127.0.0.1:${server.address().port}/backend-api"\n[model_providers.mock]\nname="Loopback mock"\nbase_url="http://127.0.0.1:${server.address().port}"\nwire_api="responses"\nrequires_openai_auth=true\nsupports_websockets=true\n`, { mode: 0o600 });
    const configPath = join(root, 'bridge.json');
    await writeFile(configPath, JSON.stringify({ codexBinary: process.env.PIAZ_REAL_CODEX, piazBinary: '/not-used' }), { mode: 0o600 });
    const env = { ...process.env, CODEX_HOME: root, PIAZ_CODEX_BRIDGE_CONFIG: configPath };
    for (const key of ['OPENAI_API_KEY', 'CODEX_API_KEY', 'OPENAI_BASE_URL']) delete env[key];
    child = spawn(process.execPath, [fileURLToPath(new URL('./codex-proxy.mjs', import.meta.url)), 'app-server'], { env, stdio: ['pipe', 'pipe', 'ignore'] });
    createInterface({ input: child.stdout }).on('line', line => {
      const message = JSON.parse(line), request = pending.get(message.id);
      if (request) { pending.delete(message.id); message.error ? request.reject(new Error(JSON.stringify(message.error))) : request.resolve(message.result); }
      if (message.method === 'turn/completed') completed.push(message.params.turn);
    });
    const rpc = (method, params) => new Promise((resolve, reject) => {
      const id = ++sequence; pending.set(id, { resolve, reject }); child.stdin.write(`${JSON.stringify({ id, method, params })}\n`);
    });
    await rpc('initialize', { clientInfo: { name: 'piaz-native-test', version: '1' }, capabilities: { experimentalApi: true } });
    child.stdin.write('{"method":"initialized"}\n');
    assert.equal((await rpc('account/read', {})).account.email, 'account-a@example.test');
    const thread = (await rpc('thread/start', { cwd: root, approvalPolicy: 'never', sandbox: 'read-only' })).thread;
    await rpc('turn/start', { threadId: thread.id, input: [{ type: 'text', text: 'Reply OK.', text_elements: [] }] });
    await waitFor(() => releaseFirst);
    assert.equal(completed.length, 0);
    const marker = join(root, 'runtime', `${child.pid}.json`);
    await waitFor(async () => JSON.parse(await readFile(marker, 'utf8')).sawTurn);
    const before = JSON.parse(await readFile(marker, 'utf8'));
    await saveLogin('account-b');
    await waitFor(async () => JSON.parse(await readFile(marker, 'utf8')).reason === 'piaz-switch');
    const after = JSON.parse(await readFile(marker, 'utf8'));
    assert.equal(after.nativePid, before.nativePid);
    assert.equal(after.bridgeRevision, 2);
    assert.notEqual(after.accountFingerprint, before.accountFingerprint);
    releaseFirst();
    await waitFor(() => completed.length === 1);
    assert.equal(completed[0].status, 'failed');
    assert.equal(completed[0].error.codexErrorInfo, 'serverOverloaded');
    assert.equal((await rpc('account/read', {})).account.email, 'account-b@example.test');
    await rpc('turn/start', { threadId: thread.id, input: [{ type: 'text', text: 'Reply OK again.', text_elements: [] }] });
    await waitFor(() => completed.length === 2);
    assert.equal(completed[1].status, 'completed');
    assert.equal(sampled[0].account, 'account-a');
    assert.equal(sampled.at(-1).account, 'account-b');
    assert.equal(sampled.at(-1).transport, 'websocket');
  } finally {
    if (child && child.exitCode === null) { const exited = new Promise(resolve => child.once('exit', resolve)); child.kill(); await exited; }
    for (const socket of sockets) socket.destroy();
    await new Promise(resolve => server.close(resolve));
    await rm(root, { recursive: true, force: true });
  }
});
