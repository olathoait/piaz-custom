# Piaz custom update recovery — 2026-11-13

Installed **0.6.1+codex.3** at `/Applications/Piaz Switch.app` and relaunched Piaz.
The official 0.6.1 bundle had replaced 0.6.0+codex.2 and omitted Codex automation
and the private `codex-bridge` command. The external T3 proxy and custom preferences
were still present.

## Changes

- Merged upstream 0.6.1 (`eea3459dc878e7936af8a5afe322ec11ffee9644`) into the existing
  custom working tree; retained upstream Claude identity and renewal fixes.
- Restored Codex automatic switching, Codex warm-up, provider-separated ledgers,
  account controls and the private bridge command.
- `+codex.*` builds refuse upstream installation in the Rust command and expose
  release viewing in the update dialog. Future upgrades require merging and
  rebuilding the custom source. A manually installed official bundle can still
  replace these features.
- Preserved Piaz preferences and enrolled accounts, active Codex auth and T3
  provider configuration: all four files' SHA-256 hashes matched before/after.
- Kept the running T3 0.1.9 and native Codex processes alive.

## Validation on macOS x86_64

- Rust workspace: **401 passed**, 3 intentionally ignored integration tests.
- Bridge and installed-native Codex loopback tests: **17 passed**, no skips.
- Clippy with `-D warnings`, Rust formatting, JavaScript syntax and diff checks passed.
- UI self-check covered the custom update explanation and release-only action.
- Release build and installed bundle passed strict/deep code-signature verification.
- Installed `codex-bridge` rejects invalid requests through its headless protocol.
- Installed bridge `account/read` matched the selected account without switching it.
- Existing live chain: T3 9401 → bridge 61564 → native Codex 61567; `sawTurn=true`,
  active-account fingerprint matches and proxy source/installed hashes match.
- Fresh Piaz usage cache: 1/1 Claude and 6/6 Codex accounts have `ok` status.

Binary SHA-256: `a6409e93a67223161ed987fb97fde0f5a2bb2928ec234bb402c076f5512b7b43`.

## Backup and limits

Private app/data/source backups, validation logs and the JSON receipt:
`/Users/thoabd/.local/share/piaz-switch-backups/20261113-212124-custom-recovery`.

The source remains in the existing checkout with its uncommitted custom changes;
no reset, stash, commit or remote push was performed. Before/after source patches
and untracked-source archives are retained in the backup.

No deliberate live quota exhaustion or new inference warm-up test was performed.
Desktop accessibility automation is disabled, so native GUI interaction was not
fully tested. Unit/loopback recovery tests and a live read-only account probe passed.
Rollback restores only an app bundle; never restore stale credential backups.
