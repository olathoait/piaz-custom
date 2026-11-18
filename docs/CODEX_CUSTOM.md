# Codex automation — local custom build

Based on upstream v0.6.1, commit eea3459dc878e7936af8a5afe322ec11ffee9644.
Build version: `0.6.1+codex.4`.

Each Codex and Claude Code account displays its usage-limit reset count below the email.
Claude Code totals the remaining resets in its eligible grants, including grants
that are temporarily paused; the dialog shows whether each grant can be used now.
Counts refresh at most once a minute while Accounts is visible; unavailable reads
are shown separately from zero. Click the count to open the existing reset dialog.

The account menu now supports earned Codex usage-limit resets as well as Claude
reset grants. See [usage limit resets](CLAUDE_LIMIT_RESETS.md) for the confirmation,
account isolation and idempotency behavior.

## Updating this custom app

The official 0.6.1 bundle replaced the earlier custom app, removing its Codex
automation and `codex-bridge` subcommand. The settings and external T3 proxy
survived. This build restores the custom features on top of 0.6.1, including
upstream's Claude identity and renewal fixes.

The updater can still check and show release notes, but cannot install an
upstream bundle over a `+codex.*` build. The release button opens the upstream
page for reference. Merge future upstream changes into this custom source,
run the Rust and bridge tests, build the macOS bundle and replace the app.
Installing an official download manually would remove these features again.

## Controls

In Settings → Automation, enable **Switch Codex automatically**. It uses the
existing threshold and account order; the Claude switch remains independent.
In Warm-up, select the Codex accounts. Both providers use the existing local-time
warm-up span, including spans across midnight. Existing settings migrate with
Codex automation off until enabled.

## Switching contract

- Uses the upstream usage refresh loop and duty loop (duties run every 60 seconds).
- Requires a quota read started within the last 120 seconds. Accounts whose latest
  read failed are excluded, even if the UI keeps their older good numbers.
- The current account must reach the configured threshold; a target needs valid,
  nonempty quota readings and every reported window below the threshold.
- Uses the existing account order, attempts-per-limit guard, encrypted vault,
  atomic auth-file writer, and unmanaged-login protection. Codex has a 60-second
  quiet period after a switch. Manually held limits remain held until reset.
- Rechecks the source account under the app's operation lock before switching.
- The standalone app changes the global login without restarting Codex or T3.
  Existing processes can retain the previous account. The T3 bridge below handles
  authentication inside a bridged provider process and continues its failed run.
- No inference request is used merely to poll quota or to force a limit.

## Warm-up contract

- Runs the installed Codex CLI with a short OK prompt, its default model, and low
  reasoning effort. CLI tested here: `0.166.0-alpha.5.1`.
- Uses an owner-only temporary CODEX_HOME and working directory, ephemeral mode,
  read-only sandbox, no inherited user config/rules, and disables shell, unified
  exec, apps, multi-agent and web search. It never runs in the project workspace.
- Removes API-key environment overrides and the auth file API key to avoid
  accidental API billing. Only subscription-token credentials are accepted.
- For the globally active account, lends a copy without its refresh token and
  never writes the live login. Expired active credentials wait for their owner.
- A parked account can refresh its credential. The result goes back into its
  encrypted vault only when its identity matches and the original vault entry
  has not changed. App switches are serialized while it is lent out.
- Uses the existing timeout (120 seconds), failure backoff and daily failure cap.
  Provider-separated ledger keys prevent same-name Claude/Codex collisions.
- Skips accounts with an active five-hour window or another exhausted limit.
  After a successful warm-up, the existing five-hour suppression prevents repeats
  while the quota endpoint catches up. This is a local cooldown, not proof that
  a warm-up starts or changes the server's reset clock. Warm-up consumes quota.
- Do not independently log in, log out or switch via a separate CLI while a
  credential is being renewed. T3 providers can retain an older account after a
  global switch. The T3 bridge uses external access tokens and delegates renewal
  to Piaz under the same cross-process lock as switching and Codex warm-up.

## T3 account handoff (step 2)

`bridge/codex-proxy.mjs` is a dependency-free Node executable proxy around the real
Codex CLI. Version/help probes pass through. For app-server JSON-RPC it:

- Watches the global auth file and applies manual Piaz switches inside the native
  process. It also synchronizes before a new turn or account read. Codex 0.166
  revokes an active turn's network context on login changes; the bridge continues
  that affected turn through T3 after the successful handoff, including token-only
  changes. It does not disable network permission checks.
- Initializes external ChatGPT access-token authentication after the normal
  initialize/initialized handshake. Refresh tokens are kept in Piaz's existing
  credential storage; native Codex requests renewal through the proxy. Renewal
  stays available if Codex auto-switch is turned off.
- Watches root turns for structured `usageLimitExceeded`, `rateLimitExceeded`, or
  terminal HTTP 429 failures. It does not rotate for 401, context errors or prose
  that happens to contain the word quota.
- Asks the signed Piaz executable to read fresh quota, skip identities already
  exhausted in this continuation, and switch to an enrolled healthy account.
  It reuses a healthy account already selected by the global 95% switch or another
  worker. Failed quota reads and expired credentials are not candidates.
- Sends `account/login/start` with external access tokens to the existing native
  Codex process, then uses T3 0.1.7's capacity continuation to keep the same T3 run,
  conversation and real native turn IDs. The interrupted native turn stays failed;
  continuation gets a new native turn ID and completes the remaining work.
- Replaces the retry's replay of the original prompt with a continuation prompt
  that tells Codex to inspect conversation/tool results and avoid repeating work.
  This reduces replay risk; it cannot guarantee exactly-once arbitrary side effects.
- Serializes simultaneous failures inside a provider process. Piaz's file lock
  also serializes separate processes, warm-up, switches and account changes.
- Honors Stop during recovery. If no usable untried account exists or recovery
  fails, it returns the original failure. It cannot manufacture more quota.

Credentials cross private stdin/stdout pipes only; they are not command-line
arguments, application logs or config files. The proxy's config contains executable
paths only. A parked provider's refresh asks Piaz for that same enrolled identity,
without changing the global account. Token persistence checks identity and source
credentials before replacing the encrypted vault entry or active auth file.

The capacity-continuation hook is a documented shortcut pinned to **T3 0.1.7**.
T3 currently allows five capacity retries; this proxy stops after four failed
identities. Revalidate after T3/Codex upgrades. Native Codex subagent turns are
forwarded unchanged; automatic handoff is proven for root turns, not native children.

### Installation on this Mac

- Piaz `0.6.1+codex.4`: `/Applications/Piaz Switch.app`.
- Production proxy and launcher: `~/.local/share/piaz-switch/bridge/`.
- T3 target: existing provider instance `codex`, `setupMode: existing`, binary path
  `~/.local/share/piaz-switch/bridge/codex`. Other settings are retained.
- `activate-when-idle.py` is a one-shot detached installer. It reads T3's database
  read-only and waits for all Codex runs, native children and background work to be
  idle for 10 seconds. It rechecks work and settings immediately before an atomic
  settings update. When supplied known legacy process signatures, it retires those
  idle processes and waits for a real T3 turn through the bridge. A new run arriving
  between the last work check and process retirement remains a race; T3 exposes no
  transactional pause-and-reconfigure hook.
- Current status is in `~/.local/share/piaz-switch/bridge/activation-status.json`.
  Normal deferred states are `waitingForIdle`, `awaitingRuntime`, then `active`
  after an actual T3 child proxy handles a turn. The installer expires after 12
  hours if no stable idle window occurs; it does not terminate running tasks.
- Corrected runtime finding: changing provider config does not retire T3's existing
  shared Codex process. It can survive for 30 minutes idle or longer while busy.
  The earlier `active` status based on a ready provider cache was a false positive.
  The installer now retires only a known idle legacy child, with PID/start/parent
  checks, and reports `active` only after a real T3 child proxy has handled a turn.
  Runtime proof files contain process IDs and an account fingerprint, never tokens.
- The user subsequently authorized immediate migration without waiting. That
  one-shot operation has completed: old native PID 89624 exited; T3 PID 88738 now
  owns proxy PID 27103, which owns native Codex PID 27104. The proxy handled this
  resumed implementation turn and its account fingerprint matches the current
  global Piaz login. Source and installed bridge files match. Only this session
  needed continuation; the other initially active conversation finished before
  migration, so it was not restarted. T3 itself was not restarted.
- Both one-shot installer workers have exited. The proxy's built-in watcher is
  responsible for subsequent Piaz account changes; no background migration loop
  or repeated restart is required.
- `settings-before.json` beside the proxy is the private settings backup made
  just before activation. For rollback, wait for Codex work to be idle and restore
  only `providerInstances.codex` from `activation-request.json`'s `previousCodex`;
  remove that override if the previous value is null. Retain other current settings
  and current credentials. Do not restore old refresh tokens.

## Validation

```sh
~/.cargo/bin/cargo fmt --all -- --check
~/.cargo/bin/cargo clippy --workspace --all-targets --locked -- -D warnings
~/.cargo/bin/cargo test --workspace --locked
node --check crates/pitstop-app/ui/app.js
node --check crates/pitstop-app/ui/i18n.js
node --test bridge/codex-proxy.test.mjs
python3 -B bridge/activate-when-idle.test.py
# Optional installed-native check; synthetic credentials and loopback inference:
PIAZ_REAL_CODEX='/Applications/ChatGPT.app/Contents/Resources/codex-cli/CodexCLI.app/Contents/MacOS/codex' \
  node --test bridge/native-account-switch.test.mjs
```

The normal suite uses fake accounts. To explicitly spend one real inference
request and prove that warm-up leaves the active auth file unchanged:

```sh
~/.cargo/bin/cargo test -p pitstop-app --locked \
  codex_warm::tests::real_codex_warmup_keeps_active_login -- --ignored --exact
```

## Build and rollback

```sh
~/.cargo/bin/cargo build --release --workspace --locked
sh crates/pitstop-app/macos/bundle.sh target/release/pitstop-app
```

Keep the previous app bundle and a private backup of `~/.pitstop` and
`~/.codex/auth.json` before replacing the app. The custom updater blocks upstream
installation; update checks can remain off or be enabled for release notices.
To roll back the executable, quit Piaz and restore only the previous app bundle.
Keep current credentials: restoring old refresh tokens can invalidate a working
login. The old app ignores the new Codex settings and provider-prefixed ledger.
Never commit account backups, vault files or auth.json to this repository.

## Verification on this Mac (2026-11-05)

- macOS 15.7.9, Intel x86_64; Rust 1.99.1 installed without changing shell PATH.
- Workspace tests: 374 passed (134 app + 240 core), no failures. The two upstream
  real-Claude tests remain ignored; the new real-Codex test is opt-in.
- The opt-in real-Codex test was run separately and passed. It completed one OK
  inference and asserted byte-for-byte equality of the live auth file before/after.
- Clippy with warnings denied, Rust formatting, both JavaScript syntax checks,
  and git diff whitespace checks passed.
- Sixteen Node proxy tests, one native integration test and one installer test passed, including Stop,
  concurrent failures, token refresh identity checks, unavailable quota, native
  child pass-through, and deferred activation preserving other settings.
- The native integration test changes synthetic authentication during an active
  Codex turn. Its account-change failure becomes a T3 continuation; the next
  WebSocket inference request on the same conversation is authenticated as B,
  with the same native process. No subscription quota is consumed by this test.
- Real T3 0.1.7 and Codex were exercised in an isolated home. Two runs each received
  an injected quota failure after real tool work, switched an enrolled account,
  continued in the same T3 run and finished. Both proof files retained exactly one
  line. A third run resumed the same conversation after a watched provider config
  reload and completed. See [sanitized evidence](CODEX_T3_EVIDENCE.json).
- The test server was stopped and its copied credentials and temporary server
  authentication logs removed. No real account was deliberately exhausted.
- The live Piaz duty loop was also observed switching at a real 99% quota reading;
  this proves the global switch, not recovery in the older unbridged T3 process.
- Long-lived token renewal is covered by protocol/unit tests, not an elapsed-time
  live refresh test. Natural-limit recovery through the production bridge remains
  unobserved at installation.

## Installed configuration (2026-11-05, historical)

- App: `/Applications/Piaz Switch.app`; native UI verified version `0.6.0+codex.2`.
- Latest private pre-step-2 backup: `/Users/thoabd/.local/share/piaz-switch-backups/20261105-020505`.
- Codex auto-switch on at 95%, Most left.
- Warm-up enabled for all 4 enrolled Codex accounts, 01:00–23:00 local time.
- Claude settings retained; upstream update checks disabled.
- Installed binary SHA-256: `07804522e193d959e4379654dde40af76dbed280156da780bdd15891487b369e`.
- Ad-hoc code signature verified. The final headless bridge probe succeeded using
  the current healthy account without printing or saving its token response.
- The user granted macOS Keychain access after the signature change. Runtime
  quota reads then succeeded for all four Codex accounts.
- Three parked Codex accounts completed automatic warm-up (activity result
  `done`, no failures). The active account was correctly skipped because its
  five-hour window was already running. The live auth file remained unchanged.
- Native UI checks verified both Codex auto-switch and warm-up off/on controls
  persisted successfully. Final state: both on, all four accounts selected.
- Warm-up and automatic switch remain on. Live T3 bridge revision 2 is active,
  verified against the actual running process chain and current Piaz account.
  Process IDs above are installation evidence and can change on a normal restart.
