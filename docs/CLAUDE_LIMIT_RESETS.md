# Claude and Codex usage limit resets

In a Claude account's menu, choose **Usage limit resets…** (**Lượt reset hạn
mức…**). Piaz checks that account, including parked accounts, without switching
the global login. Available offers show the reset count and expiry date. Choose
**Reset for free**, review which limits it refills, then confirm with **Reset for
free**. Opening the dialog never spends a reset.

Claude must have granted the account an eligible, usable reset. Piaz cannot create
one. Paused or currently unusable offers stay disabled. An expired or refused
login needs renewal or sign-in before it can be checked.

Each redemption verifies the token's owner and organization, checks fresh offer
availability and sends one POST with a request ID kept for retries in that dialog.
It then records the result in Activity and refreshes usage. If the response is
lost, check usage before trying again: the reset may already have been consumed.

This uses the private API observed in the locally installed Claude Code
**2.1.325**:

- `GET https://api.anthropic.com/api/oauth/profile` verifies the account.
- `GET https://api.anthropic.com/api/oauth/usage?cedar_ember=1&skip_spend=1`
  reads the `cedar_ember` grants.
- `POST https://api.anthropic.com/api/organizations/{organization}/reset_rate_limits`
  sends `program: cedar_ember`, `grant_id` and `request_id`.

These endpoints may change. No reset is claimed during background usage polling.
Anthropic's user-facing behavior is documented in
[What is a limit reset?](https://support.claude.com/en/articles/17007452-what-is-a-limit-reset).

## Codex earned resets

Codex accounts have the same menu item. Piaz uses the documented
[Codex app-server reset API](https://learn.chatgpt.com/docs/app-server#8-earned-rate-limit-resets-chatgpt),
without changing the account selected globally or interrupting a T3 turn.

- `account/rateLimits/read` supplies `rateLimitResetCredits.availableCount` and
  optional details. The count is authoritative. When details are absent or capped,
  **Next available reset** lets Codex choose the next credit, without inventing an ID
  or expiry date. An unknown response is an error, distinct from zero resets.
- `account/rateLimitResetCredit/consume` receives a UUID `idempotencyKey` and, when
  selected, the opaque `creditId`. Manual retries in the same dialog retain both.
  `reset` and `alreadyRedeemed` are successful outcomes; `nothingToReset` and
  `noCredit` are shown as failures. Piaz then requests updated rate limits.
- The native Codex CLI runs in an empty private home and receives only access
  tokens over stdin. No refresh token or API key is lent, no inference is started,
  and the temporary process and home are removed after the operation.
- Account identity and workspace must still match the selected account. Expired
  logins require renewal first. Neither checking nor background polling spends a
  reset. Account grants and eligible windows are determined by Codex.

The protocol was verified with installed CLI `0.166.0-alpha.5.1` against a loopback
API and synthetic credentials. The check covered detailed credit reads, all four
redemption outcomes, request-ID forwarding, and absence of a saved auth file.
It consumed no real reset or subscription inference quota.

Offline checks (fake credentials; no real resets consumed):

```sh
cargo test --workspace --locked
node --test crates/pitstop-app/tests/limit-reset-ui.test.cjs
# Installed CLI, fake account and loopback API only:
PIAZ_REAL_CODEX=/path/to/native/codex python3 -B bridge/native-limit-reset.test.py
```
