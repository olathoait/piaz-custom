// Uses T3 0.1.7's existing capacity continuation; native turn IDs/history stay intact.
import { spawn } from 'node:child_process';
import { readFile, mkdir, writeFile, rename, unlink } from 'node:fs/promises';
import { watchFile, unwatchFile } from 'node:fs';
import { createInterface } from 'node:readline';
import { homedir } from 'node:os';
import { join } from 'node:path';
import { randomUUID, createHash } from 'node:crypto';
import { fileURLToPath } from 'node:url';

export const CONTINUE = 'Continue the interrupted task from the existing conversation and tool results. The Codex authentication changed. Inspect existing state before acting; do not repeat completed commands, writes, payments, deployments or other completed operations. Complete only the remaining work.';
const retryError = { message: 'Piaz updated Codex authentication; T3 will continue the same run.', codexErrorInfo: 'serverOverloaded', additionalDetails: null };
export function quotaError(error) {
  const info = error?.codexErrorInfo;
  return info === 'usageLimitExceeded' || info === 'rateLimitExceeded'
    || info?.responseTooManyFailedAttempts?.httpStatusCode === 429;
}
export function authIdentity(auth) {
  try {
    const claims = JSON.parse(Buffer.from(auth.tokens.id_token.split('.')[1], 'base64url'));
    const openai = claims['https://api.openai.com/auth'] ?? {};
    const user = openai.chatgpt_user_id ?? claims.sub;
    const account = openai.chatgpt_account_id ?? auth.tokens.account_id ?? '';
    return typeof user === 'string' ? `${user}:${account}` : null;
  } catch { return null; }
}

export class CodexProxy {
  constructor({ toNative, toClient, recover, identity = null, initialAuth = null, readLogin = null, onAccount = () => {}, warn = () => {} }) {
    Object.assign(this, { toNative, toClient, recover, identity, initialAuth, readLogin, onAccount, warn });
    this.accessToken = initialAuth?.accessToken;
    this.internal = new Map();
    this.starts = new Map();
    this.turns = new Map();
    this.continuations = new Map();
    this.queue = Promise.resolve();
    this.externalAuth = false;
    this.authEpoch = 0;
  }
  request(method, params, timeout = 15000) {
    const id = `piaz:${randomUUID()}`;
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => { this.internal.delete(id); reject(new Error('Codex bridge RPC timeout')); }, timeout);
      this.internal.set(id, { resolve, reject, timer });
      this.toNative({ id, method, params });
    });
  }
  client(message) {
    if (message.method === 'initialized' && this.initialAuth) {
      this.toNative(message);
      const auth = this.initialAuth; this.initialAuth = null;
      // Piaz owns refresh tokens; native Codex receives access tokens only.
      this.authReady = this.request('account/login/start', auth).then(() => {
        this.externalAuth = true; this.onAccount(this.identity, 'startup');
      });
      this.authReady.catch(() => this.warn('Piaz could not initialize external account authentication.'));
      return;
    }
    if (this.authReady && message.method && message.method !== 'initialize') {
      this.authReady.then(() => { if (!this.closed) this.dispatch(message); }, () => {
        if (message.id !== undefined) this.toClient({ id: message.id, error: { code: -32000, message: 'Piaz account authentication unavailable' } });
      });
      return;
    }
    this.dispatch(message);
  }
  dispatch(message) {
    if (this.readLogin && ['turn/start', 'account/read'].includes(message.method)) {
      this.sync().then(() => { if (!this.closed) this.forward(message); }, () => {
        if (message.id !== undefined) this.toClient({ id: message.id, error: { code: -32000, message: 'Piaz could not apply the selected account' } });
      });
    } else this.forward(message);
  }
  sync() {
    const task = this.queue.catch(() => {}).then(async () => {
      if (this.closed || !this.externalAuth || !this.readLogin) return;
      const selected = await this.readLogin();
      if (!selected || (selected.identity === this.identity && selected.auth.accessToken === this.accessToken)) return;
      await this.adopt(selected, 'piaz-switch');
    });
    this.queue = task.catch(() => {});
    return task;
  }
  async adopt(selected, reason) {
    // Codex revokes an active turn's network context when account/login/start runs.
    for (const state of [...this.starts.values(), ...this.turns.values()]) state.authHandoff = true;
    await this.request('account/login/start', selected.auth);
    this.identity = selected.identity; this.accessToken = selected.auth.accessToken;
    this.authEpoch++;
    this.externalAuth = true; this.onAccount(this.identity, reason);
  }
  forward(message) {
    if (message.method === 'initialize') {
      message = structuredClone(message);
      message.params ??= {};
      message.params.capabilities = { ...message.params.capabilities, experimentalApi: true };
    }
    if (message.method === 'turn/start') {
      const thread = message.params.threadId;
      const continuation = this.continuations.get(thread);
      if (continuation) {
        message = structuredClone(message);
        message.params.input = [{ type: 'text', text: CONTINUE, text_elements: [] }];
        this.continuations.delete(thread);
      }
      this.starts.set(thread, {
        thread, identity: this.identity, authEpoch: this.authEpoch, excluded: continuation?.excluded ?? new Set(),
        compact: message.params.input?.some(i => i.type === 'text' && i.text?.trim().toLowerCase() === '/compact'),
        cancelled: false, pending: false,
      });
    }
    if (message.method === 'turn/interrupt') {
      const state = this.turns.get(message.params.turnId);
      this.continuations.delete(message.params.threadId);
      if (state) state.cancelled = true;
      if (state?.pending) {
        // The native turn is already terminal; honour Stop while account recovery runs.
        state.pending = false;
        this.toClient({ id: message.id, result: {} });
        this.toClient({ ...state.terminal, params: { ...state.terminal.params, turn: { ...state.terminal.params.turn, status: 'interrupted', error: null } } });
        this.turns.delete(message.params.turnId);
        return;
      }
    }
    this.toNative(message);
  }
  native(message) {
    const pending = this.internal.get(message.id);
    if (pending) {
      clearTimeout(pending.timer); this.internal.delete(message.id);
      if (message.error) pending.reject(new Error('Codex rejected bridge authentication'));
      else pending.resolve(message.result);
      return;
    }
    if (this.externalAuth && message.method === 'account/chatgptAuthTokens/refresh' && message.id !== undefined) {
      this.refresh(message); return;
    }
    if (message.method === 'turn/started') {
      const state = this.starts.get(message.params.threadId);
      if (state) {
        this.turns.set(message.params.turn.id, state); this.starts.delete(state.thread);
        this.onAccount(this.identity, 'turn-started');
      }
    }
    const turnId = message.params?.turnId ?? message.params?.turn?.id;
    const state = this.turns.get(turnId);
    if (state && !state.compact && !state.cancelled && message.method === 'error'
      && !message.params.willRetry && (quotaError(message.params.error) || state.authHandoff)) {
      state.error = message; state.failedIdentity = state.identity; return;
    }
    if (state && message.method === 'turn/completed') {
      if (!state.compact && !state.cancelled && message.params.turn.status === 'failed'
        && (quotaError(message.params.turn.error) || quotaError(state.error?.params.error) || state.authHandoff)) {
        state.failedIdentity ??= state.identity;
        state.terminal = message; state.pending = true;
        this.queue = this.queue.catch(() => {}).then(() => this.rotate(state, turnId));
        return;
      }
      if (state.error) this.toClient(state.error);
      this.turns.delete(turnId);
    }
    this.toClient(message);
  }
  async rotate(state, turnId) {
    try {
      if (state.cancelled || !state.failedIdentity || state.excluded.size >= 4) throw new Error('Recovery is unavailable');
      if (quotaError(state.terminal.params.turn.error) || quotaError(state.error?.params.error)) {
        state.excluded.add(state.failedIdentity);
        if (this.identity === state.failedIdentity) {
          const selected = await this.recover({ excludedIdentities: [...state.excluded], refresh: false });
          if (state.cancelled) return;
          if (!selected.ok || state.excluded.has(selected.identity)) throw new Error('No fresh account');
          await this.adopt(selected, 'quota');
          if (state.cancelled) return;
        }
        if (state.excluded.has(this.identity)) throw new Error('Account already exhausted');
      } else if (this.authEpoch === state.authEpoch) {
        throw new Error('Account handoff did not succeed');
      }
      if (state.cancelled) return;
      this.continuations.set(state.thread, { excluded: state.excluded });
      // shortcut: T3 0.1.7 exposes continuation through serverOverloaded only; replace with a native account-handoff hook when T3 provides one.
      this.toClient({ method: 'error', params: { threadId: state.thread, turnId, error: retryError, willRetry: false } });
      this.toClient({ ...state.terminal, params: { ...state.terminal.params, turn: { ...state.terminal.params.turn, error: retryError } } });
    } catch {
      if (!state.cancelled) {
        this.warn('Piaz recovery unavailable; returning the original quota failure.');
        if (state.error) this.toClient(state.error);
        this.toClient(state.terminal);
      }
    } finally {
      state.pending = false; this.turns.delete(turnId);
    }
  }
  async refresh(message) {
    try {
      const selected = await this.recover({ excludedIdentities: [], refresh: true, identity: this.identity });
      if (!selected.ok || selected.identity !== this.identity) throw new Error('Refresh identity mismatch');
      const { accessToken, chatgptAccountId } = selected.auth;
      this.accessToken = accessToken;
      this.toNative({ id: message.id, result: { accessToken, chatgptAccountId, chatgptPlanType: null } });
    } catch { this.toNative({ id: message.id, error: { code: -32000, message: 'Piaz could not refresh this Codex account' } }); }
  }
  close() {
    this.closed = true;
    for (const p of this.internal.values()) { clearTimeout(p.timer); p.reject(new Error('Codex exited')); }
    this.internal.clear();
    for (const s of this.turns.values()) s.cancelled = true;
  }
}

function bridgeCall(binary, request) {
  return new Promise((resolve, reject) => {
    const child = spawn(binary, ['codex-bridge'], { stdio: ['pipe', 'pipe', 'ignore'] });
    let output = ''; let timedOut = false;
    // Never kill an account refresh halfway through persisting a rotated token.
    const timer = setTimeout(() => { timedOut = true; reject(new Error('Piaz recovery timed out')); }, 180000);
    child.stdout.on('data', data => {
      if (output.length + data.length > 65536) { reject(new Error('Oversized bridge response')); return; }
      output += data;
    });
    child.on('error', () => { clearTimeout(timer); reject(new Error('Piaz bridge unavailable')); });
    child.on('close', code => {
      clearTimeout(timer);
      if (timedOut) return;
      try { const result = JSON.parse(output); if (code !== 0 || !result.ok) throw new Error(); resolve(result); }
      catch { reject(new Error('Piaz recovery failed')); }
    });
    child.stdin.on('error', () => {});
    child.stdin.end(JSON.stringify(request));
  });
}

async function main() {
  const configPath = process.env.PIAZ_CODEX_BRIDGE_CONFIG ?? join(homedir(), '.local/share/piaz-switch/bridge/config.json');
  const config = JSON.parse(await readFile(configPath, 'utf8'));
  const args = process.argv.slice(2);
  const proxied = args.includes('app-server') && !args.includes('--help') && !args.includes('-h');
  if (!proxied) {
    const child = spawn(config.codexBinary, args, { stdio: 'inherit' });
    child.on('error', () => process.exit(127));
    child.on('exit', code => process.exit(code ?? 1));
    return;
  }
  const child = spawn(config.codexBinary, args, { stdio: ['pipe', 'pipe', 'inherit'] });
  const authPath = join(process.env.CODEX_HOME ?? join(homedir(), '.codex'), 'auth.json');
  const readLogin = async () => {
    const auth = JSON.parse(await readFile(authPath, 'utf8'));
    const identity = authIdentity(auth);
    if (identity && auth.tokens?.access_token && auth.tokens?.account_id) {
      return { identity, auth: { type: 'chatgptAuthTokens', accessToken: auth.tokens.access_token, chatgptAccountId: auth.tokens.account_id } };
    }
    return null;
  };
  const initial = await readLogin().catch(() => null);
  const runtimeDir = join(configPath, '..', 'runtime');
  const runtimePath = join(runtimeDir, `${process.pid}.json`);
  let sawTurn = false, statusWrites = Promise.resolve();
  const onAccount = (identity, reason) => {
    sawTurn ||= reason === 'turn-started';
    const record = { bridgeRevision: 2, pid: process.pid, parentPid: process.ppid, nativePid: child.pid, sawTurn,
      accountFingerprint: createHash('sha256').update(identity ?? '').digest('hex').slice(0, 16),
      reason, updatedAt: new Date().toISOString() };
    statusWrites = statusWrites.catch(() => {}).then(async () => {
      await mkdir(runtimeDir, { recursive: true, mode: 0o700 });
      await writeFile(`${runtimePath}.tmp`, JSON.stringify(record), { mode: 0o600 });
      await rename(`${runtimePath}.tmp`, runtimePath);
    }).catch(() => {});
  };
  const proxy = new CodexProxy({
    identity: initial?.identity, initialAuth: initial?.auth, readLogin, onAccount,
    toNative: message => child.stdin.write(`${JSON.stringify(message)}\n`),
    toClient: message => process.stdout.write(`${JSON.stringify(message)}\n`),
    recover: request => bridgeCall(config.piazBinary, request),
    warn: text => process.stderr.write(`${text}\n`),
  });
  const input = createInterface({ input: process.stdin });
  const output = createInterface({ input: child.stdout });
  watchFile(authPath, { interval: 1000, persistent: false }, () => {
    proxy.sync().catch(() => proxy.warn('Piaz could not synchronize the selected account.'));
  });
  child.stdin.on('error', () => {});
  input.on('line', line => { try { proxy.client(JSON.parse(line)); } catch { process.stderr.write('Invalid client JSON-RPC message\n'); } });
  output.on('line', line => { try { proxy.native(JSON.parse(line)); } catch { process.stderr.write('Invalid native JSON-RPC message\n'); } });
  input.on('close', () => { proxy.close(); child.stdin.end(); child.kill(); });
  child.on('error', () => { proxy.close(); process.exit(127); });
  child.on('exit', async code => {
    unwatchFile(authPath); proxy.close();
    await statusWrites.catch(() => {}); await unlink(runtimePath).catch(() => {});
    process.exit(code ?? 1);
  });
  for (const signal of ['SIGTERM', 'SIGINT']) process.on(signal, () => { proxy.close(); child.kill(signal); });
}
if (process.argv[1] === fileURLToPath(import.meta.url)) main().catch(() => { process.stderr.write('Piaz Codex bridge could not start\n'); process.exit(1); });
