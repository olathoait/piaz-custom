# Immediate runtime migration — 2026-11-05

The user reported T3 retaining the old Codex login, then explicitly requested
`thay đổi luôn không đợi` (apply now without waiting). They authorized replacing
the shared Codex process immediately and continuing affected existing tasks.

## Completed

- Found T3's shared direct `codex app-server` process survived the earlier config
  update. The earlier ready-cache activation status was a false positive.
- Installed bridge revision 2 in `~/.local/share/piaz-switch/bridge/`.
- Bridge watches auth.json (1s polling), synchronizes before turn/start and
  account/read, and passes external access tokens to native Codex.
- Real native test exposed that changing account during an active turn revokes
  that turn's network context. Bridge now uses T3's capacity continuation after
  a successful auth handoff, including token-only updates. It retains permission
  checks and uses a fresh native turn, with a continuation prompt avoiding replay.
- Runtime markers under `bridge/runtime/<pid>.json` contain only PID, parent/native
  PID, revision, sawTurn and an account fingerprint. Activation checks actual
  T3 child/native process relationships, not a provider status probe.
- Latest tests: 17 Node tests passed (16 proxy + 1 actual installed Codex using
  synthetic auth and loopback WebSocket inference), plus installer test passed.
  Installed Rust app remains `.2`; its unchanged latest suite had 374 passing tests.
- Native integration proves account change during an active turn, transformed
  terminal failure to T3 continuation, then B's auth header on the same
  conversation's next WebSocket request, without replacing the native process.
- Bridge backup: `/Users/thoabd/.local/share/piaz-switch-backups/20261105-025825-bridge`.

## Migration completed and verified

One-shot `~/.local/share/piaz-switch/bridge/migrate-now.py` retired the exact known
legacy T3 child PID 89624 and continued this implementation conversation. The
other initially active conversation had completed before migration, so it was
not sent a duplicate continuation. No new top-level thread was created.

Live verification after resumption confirmed:

- T3 server PID 88738 → Node proxy PID 27103 → native Codex PID 27104.
- Revision 2 marker says `sawTurn: true`; account fingerprint matches the current
  Piaz login. The proxy's command and both parent relationships were checked live.
- Source hashes equal installed proxy/installer hashes, live T3 settings point to
  the bridge launcher, and the Piaz app's signature verifies.
- The other conversation was read through T3 and confirmed completed; this session
  resumed successfully through the bridge. Both one-shot workers have exited.
- `CODEX_T3_EVIDENCE.json` and `CODEX_CUSTOM.md` now record the verified result.

Do not start another migration. The installed watcher handles subsequent account
changes. Natural production quota recovery, long-lived token renewal and native
subagent automatic recovery remain unproven. Preserve existing checkout/diffs.
