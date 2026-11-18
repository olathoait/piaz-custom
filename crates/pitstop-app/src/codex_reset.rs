//! Read and redeem Codex's earned resets in an isolated, access-token-only server.
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use pitstop_core::limit_reset::{Grant, Status};
use pitstop_core::provider::provider_for;
use pitstop_core::signin::{self, PrivateDir};
use pitstop_core::{Engine, Host, ProviderId};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{app, codex_warm, t3_bridge};

const UNCERTAIN: &str =
    "Could not confirm the Codex reset. Check usage before retrying with the same request id";

struct Server {
    child: Child,
    input: ChildStdin,
    output: Receiver<Value>,
    sequence: u32,
    _home: PrivateDir,
}

impl Server {
    fn start(program: &Path) -> Result<Self, String> {
        let home = PrivateDir::create_for(ProviderId::Codex, "limit-reset")
            .map_err(|_| "Could not create an isolated Codex home")?;
        let mut child = Command::new(program)
            .args(["-c", "cli_auth_credentials_store=\"file\"", "app-server"])
            .env("CODEX_HOME", home.path())
            .env_remove("OPENAI_API_KEY")
            .env_remove("CODEX_API_KEY")
            .env_remove("OPENAI_BASE_URL")
            .current_dir(home.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|_| "Could not start Codex for limit resets")?;
        let input = child.stdin.take().expect("piped stdin");
        let output = child.stdout.take().expect("piped stdout");
        let (tx, rx) = mpsc::sync_channel(32);
        std::thread::spawn(move || {
            for line in BufReader::new(output).lines().map_while(Result::ok) {
                if let Ok(value) = serde_json::from_str::<Value>(&line) {
                    if tx.send(value).is_err() {
                        break;
                    }
                }
            }
        });
        Ok(Self {
            child,
            input,
            output: rx,
            sequence: 0,
            _home: home,
        })
    }

    fn send(&mut self, message: Value) -> Result<(), String> {
        writeln!(self.input, "{message}").map_err(|_| UNCERTAIN.to_owned())
    }

    fn request(&mut self, method: &str, params: Value) -> Result<Value, String> {
        self.sequence += 1;
        let id = self.sequence;
        self.send(json!({"id":id,"method":method,"params":params}))?;
        let until = Instant::now() + Duration::from_secs(30);
        loop {
            let message = self
                .output
                .recv_timeout(until.saturating_duration_since(Instant::now()))
                .map_err(|_| UNCERTAIN.to_owned())?;
            if message.get("method").is_none() && message["id"] == id {
                if message.get("error").is_some() {
                    if method == "account/rateLimitResetCredit/consume" {
                        return Err(UNCERTAIN.into());
                    }
                    return Err("Codex refused the request. Check the login and update Codex if reset methods are unsupported".into());
                }
                return message
                    .get("result")
                    .cloned()
                    .ok_or_else(|| UNCERTAIN.into());
            }
            if message.get("id").is_some() && message.get("method").is_some() {
                // Renewal belongs to Piaz/the live process, never this disposable server.
                self.send(json!({"id":message["id"],"error":{"code":-32000,"message":"Renew the account in Piaz before checking resets"}}))?;
            }
        }
    }

    fn login(&mut self, auth: Value) -> Result<(), String> {
        self.request("initialize", json!({"clientInfo":{"name":"piaz-limit-reset","version":"1"},"capabilities":{"experimentalApi":true}}))?;
        self.send(json!({"method":"initialized"}))?;
        self.request("account/login/start", auth)?;
        Ok(())
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Credits {
    available_count: u32,
    credits: Option<Vec<Credit>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Credit {
    id: String,
    status: String,
    title: Option<String>,
    description: Option<String>,
    expires_at: Option<i64>,
}

fn status(answer: Value, identity: String, now: i64) -> Result<Status, String> {
    let credits: Credits = serde_json::from_value(answer["rateLimitResetCredits"].clone())
        .map_err(|_| "Codex did not report reset availability. Update Codex or check the account's usage page")?;
    let mut grants = Vec::new();
    for credit in credits.credits.unwrap_or_default() {
        if credit.id.is_empty()
            || credit.status != "available"
            || credit.expires_at.is_some_and(|at| at <= now)
        {
            continue;
        }
        let ends_at = credit
            .expires_at
            .map(|at| {
                chrono::DateTime::from_timestamp(at, 0)
                    .map(|time| time.to_rfc3339())
                    .ok_or("Codex returned an invalid reset expiry")
            })
            .transpose()?;
        grants.push(Grant {
            id: credit.id,
            label: credit
                .title
                .filter(|s| !s.is_empty())
                .or(credit.description)
                .unwrap_or_default(),
            resets_left: 1,
            ends_at,
            clears: Vec::new(),
            usable_now: credits.available_count > 0,
            paused: false,
        });
    }
    if credits.available_count as usize > grants.len() {
        // Details can be capped; the service can choose the next credit without an id.
        grants.push(Grant {
            id: String::new(),
            label: String::new(),
            resets_left: credits.available_count,
            ends_at: None,
            clears: Vec::new(),
            usable_now: true,
            paused: false,
        });
    }
    Ok(Status {
        identity,
        available_count: Some(credits.available_count),
        eligible: credits.available_count > 0,
        grants,
    })
}

fn credential<H: Host>(
    engine: &Engine<H>,
    name: &str,
    expected: Option<&str>,
) -> Result<(String, Value), String> {
    let (blob, in_use) = t3_bridge::account_blob(engine.host(), name)?;
    let provider = provider_for(ProviderId::Codex);
    let identity = provider
        .details(&blob)
        .identity
        .ok_or("This Codex login has no account identity")?;
    let account = engine
        .state()
        .map_err(|_| "Could not read accounts")?
        .accounts
        .into_iter()
        .find(|a| a.provider == ProviderId::Codex && a.name == name)
        .ok_or("Account is no longer enrolled")?;
    if account.identity.as_ref() != Some(&identity) || expected.is_some_and(|id| id != identity) {
        return Err("The selected Codex account changed; check its resets again".into());
    }
    let token = codex_warm::token_for(engine.host(), name, in_use)
        .filter(|t| !t.lapsed(app::now()))
        .ok_or("Codex login is missing or expired; renew it first")?;
    let auth: Value = serde_json::from_slice(&blob).map_err(|_| "Invalid Codex login")?;
    let account_id = auth["tokens"]["account_id"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or("Codex login has no account id")?;
    if auth["tokens"]["access_token"].as_str() != Some(token.access.as_str())
        || identity.rsplit_once(':').map(|(_, workspace)| workspace) != Some(account_id)
    {
        return Err("Codex login changed during the read; check its resets again".into());
    }
    Ok((
        identity,
        json!({"type":"chatgptAuthTokens","accessToken":token.access,"chatgptAccountId":account_id}),
    ))
}

pub fn read(name: &str) -> Result<Status, String> {
    let (identity, auth) = credential(&app::engine(), name, None)?;
    let program =
        signin::find_program(ProviderId::Codex, &app::real_home()).ok_or("Codex CLI is missing")?;
    let mut server = Server::start(&program)?;
    server.login(auth)?;
    status(
        server.request("account/rateLimits/read", json!({}))?,
        identity,
        app::now(),
    )
}

fn consume(server: &mut Server, grant: &str, request: &str) -> Result<(), String> {
    if request.trim().is_empty() || request.len() > 128 || grant.len() > 1024 {
        return Err("Invalid Codex reset credit or request id".into());
    }
    let mut params = json!({"idempotencyKey":request});
    if !grant.is_empty() {
        params["creditId"] = json!(grant);
    }
    // Keep the same idempotency key on manual retry, including after a lost response.
    let answer = server.request("account/rateLimitResetCredit/consume", params)?;
    match answer["outcome"].as_str() {
        Some("reset" | "alreadyRedeemed") => {
            let _ = server.request("account/rateLimits/read", json!({}));
            Ok(())
        }
        Some("nothingToReset") => {
            Err("Codex has no eligible limit window to reset right now".into())
        }
        Some("noCredit") => Err("This Codex account has no reset credits left".into()),
        _ => Err(UNCERTAIN.into()),
    }
}

pub fn redeem(name: &str, identity: &str, grant: &str, request: &str) -> Result<(), String> {
    let (_, auth) = credential(&app::engine(), name, Some(identity))?;
    let program =
        signin::find_program(ProviderId::Codex, &app::real_home()).ok_or("Codex CLI is missing")?;
    let mut server = Server::start(&program)?;
    server.login(auth)?;
    consume(&mut server, grant, request)
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
    use pitstop_core::testing::FakeHost;

    fn credit(id: &str) -> Value {
        json!({"id":id,"status":"available","title":"Free reset","expiresAt":app::now()+3600})
    }

    #[test]
    fn reset_counts_are_authoritative_even_when_details_are_missing_or_capped() {
        for details in [Value::Null, json!([]), json!([credit("opaque:id/1")])] {
            let status = status(
                json!({"rateLimitResetCredits":{"availableCount":3,"credits":details}}),
                "a:personal".into(),
                app::now(),
            )
            .unwrap();
            assert_eq!(status.available_count, Some(3));
            assert!(status.eligible);
            assert!(status
                .grants
                .iter()
                .any(|g| g.id.is_empty() && g.resets_left == 3));
            assert_eq!(status.identity, "a:personal");
        }
    }

    #[test]
    fn reset_details_preserve_opaque_ids_and_expiry_and_skip_unavailable_rows() {
        let mut expired = credit("expired");
        expired["expiresAt"] = json!(1);
        let mut used = credit("used");
        used["status"] = json!("redeemed");
        let status = status(json!({"rateLimitResetCredits":{"availableCount":1,"credits":[credit("opaque:id/1"),expired,used]}}), "a:personal".into(), app::now()).unwrap();
        assert_eq!(status.grants.len(), 1);
        assert_eq!(status.grants[0].id, "opaque:id/1");
        assert!(status.grants[0].ends_at.is_some());
        assert!(status.grants[0].usable_now);
    }

    #[test]
    fn zero_credits_is_empty_but_unknown_or_malformed_availability_is_an_error() {
        let empty = status(
            json!({"rateLimitResetCredits":{"availableCount":0,"credits":[]}}),
            "a:personal".into(),
            app::now(),
        )
        .unwrap();
        assert!(!empty.eligible);
        assert!(empty.grants.is_empty());
        for answer in [
            json!({}),
            json!({"rateLimitResetCredits":null}),
            json!({"rateLimitResetCredits":{"availableCount":-1}}),
        ] {
            assert!(status(answer, "a:personal".into(), app::now()).is_err());
        }
    }

    fn enrolled() -> Engine<FakeHost> {
        let engine = Engine::new(FakeHost::new());
        for user in ["a", "b"] {
            let claims = URL_SAFE_NO_PAD.encode(json!({"sub":user,"exp":app::now()+3600,
                "https://api.openai.com/auth":{"chatgpt_user_id":user,"chatgpt_account_id":"personal"}
            }).to_string());
            let blob = serde_json::to_vec(&json!({"OPENAI_API_KEY":"UNUSED-API-KEY","tokens":{
                "id_token":format!("header.{claims}.sig"),"access_token":format!("header.{claims}.sig"),
                "account_id":"personal","refresh_token":"NEVER-LEND-REFRESH"
            }})).unwrap();
            provider_for(ProviderId::Codex)
                .write_active(engine.host(), &blob)
                .unwrap();
            engine.enroll(ProviderId::Codex, user).unwrap();
        }
        engine
    }

    #[test]
    fn active_and_parked_credentials_lend_only_access_tokens_and_preserve_the_global_login() {
        let engine = enrolled();
        let provider = provider_for(ProviderId::Codex);
        let before = provider.read_active(engine.host()).unwrap().unwrap().blob;
        for name in ["a", "b"] {
            let (identity, auth) = credential(&engine, name, None).unwrap();
            assert_eq!(identity, format!("{name}:personal"));
            assert_eq!(auth["type"], "chatgptAuthTokens");
            assert_eq!(auth["chatgptAccountId"], "personal");
            assert_eq!(auth.as_object().unwrap().len(), 3);
            assert!(!auth.to_string().contains("NEVER-LEND-REFRESH"));
            assert!(!auth.to_string().contains("UNUSED-API-KEY"));
        }
        assert_eq!(
            provider.read_active(engine.host()).unwrap().unwrap().blob,
            before
        );
        assert!(credential(&engine, "a", Some("b:personal")).is_err());
        assert!(credential(&engine, "missing", None).is_err());
    }

    #[test]
    fn expired_or_workspace_mismatched_codex_credentials_are_rejected() {
        let engine = enrolled();
        let provider = provider_for(ProviderId::Codex);
        let original = provider.read_active(engine.host()).unwrap().unwrap().blob;
        let mut auth: Value = serde_json::from_slice(&original).unwrap();
        let expired = URL_SAFE_NO_PAD.encode(json!({"sub":"b","exp":1}).to_string());
        auth["tokens"]["access_token"] = json!(format!("header.{expired}.sig"));
        provider
            .write_active(engine.host(), &serde_json::to_vec(&auth).unwrap())
            .unwrap();
        assert!(credential(&engine, "b", None).is_err());
        auth = serde_json::from_slice(&original).unwrap();
        auth["tokens"]["account_id"] = json!("other-workspace");
        provider
            .write_active(engine.host(), &serde_json::to_vec(&auth).unwrap())
            .unwrap();
        assert!(credential(&engine, "b", None).is_err());
    }

    #[cfg(unix)]
    fn fake_server(outcome: Option<&str>) -> (tempfile::TempDir, Server) {
        use std::os::unix::fs::PermissionsExt;
        let fixture = tempfile::tempdir().unwrap();
        let program = fixture.path().join("codex");
        let script = format!(
            r#"#!/usr/bin/env python3
import json, os, pathlib, sys
outcome = {outcome}
home = pathlib.Path(os.environ['CODEX_HOME'])
assert not (home / 'auth.json').exists()
assert not (home / 'config.toml').exists()
assert str(home) == os.getcwd()
for line in sys.stdin:
    message = json.loads(line)
    method = message.get('method')
    params = message.get('params', {{}})
    record = {{'method': method, 'params': params if method != 'account/login/start' else {{'type': params['type']}}}}
    with (home / 'calls.jsonl').open('a') as f:
        f.write(json.dumps(record)+'\n')
    if 'id' not in message:
        continue
    if method == 'initialize':
        result = {{}}
    elif method == 'account/login/start':
        assert set(params) == {{'type', 'accessToken', 'chatgptAccountId'}}
        result = {{'type':'chatgptAuthTokens'}}
    elif method == 'account/rateLimits/read':
        if outcome == 'refresh':
            print(json.dumps({{'id':message['id'], 'method':'account/chatgptAuthTokens/refresh', 'params':{{}}}}), flush=True)
            denied = json.loads(next(sys.stdin))
            assert 'error' in denied and 'result' not in denied
        result = {{'rateLimitResetCredits': {{'availableCount':2, 'credits':None}}}}
    elif method == 'account/rateLimitResetCredit/consume':
        if outcome is None:
            break
        result = {{'outcome':outcome}}
    else:
        raise AssertionError('Unexpected method')
    print(json.dumps({{'id':message['id'],'result':result}}), flush=True)
"#,
            outcome = outcome.map(|s| format!("{s:?}")).unwrap_or("None".into())
        );
        std::fs::write(&program, script).unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut server = Server::start(&program).unwrap();
        server.login(json!({"type":"chatgptAuthTokens","accessToken":"TEST-ACCESS","chatgptAccountId":"personal"})).unwrap();
        (fixture, server)
    }

    #[cfg(unix)]
    fn calls(server: &Server) -> Vec<Value> {
        std::fs::read_to_string(server._home.path().join("calls.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    #[test]
    #[cfg(unix)]
    fn checking_uses_an_isolated_server_and_never_requests_a_reset_or_inference() {
        let (_fixture, mut server) = fake_server(Some("reset"));
        server
            .request("account/rateLimits/read", json!({}))
            .unwrap();
        let home = server._home.path().to_owned();
        assert_eq!(
            calls(&server)
                .iter()
                .map(|v| v["method"].as_str().unwrap())
                .collect::<Vec<_>>(),
            [
                "initialize",
                "initialized",
                "account/login/start",
                "account/rateLimits/read"
            ]
        );
        assert!(!home.join("auth.json").exists());
        drop(server);
        assert!(!home.exists());
    }

    #[test]
    #[cfg(unix)]
    fn disposable_server_refuses_token_renewal_even_if_native_request_ids_overlap() {
        let (_fixture, mut server) = fake_server(Some("refresh"));
        let answer = server
            .request("account/rateLimits/read", json!({}))
            .unwrap();
        assert_eq!(answer["rateLimitResetCredits"]["availableCount"], 2);
    }

    #[test]
    #[cfg(unix)]
    fn consumption_preserves_the_credit_and_idempotency_key_and_refreshes_after_both_success_outcomes(
    ) {
        for outcome in ["reset", "alreadyRedeemed"] {
            let (_fixture, mut server) = fake_server(Some(outcome));
            consume(&mut server, "opaque:id/1", "stable-request").unwrap();
            let calls = calls(&server);
            let claims: Vec<_> = calls
                .iter()
                .filter(|v| v["method"] == "account/rateLimitResetCredit/consume")
                .collect();
            assert_eq!(claims.len(), 1);
            assert_eq!(
                claims[0]["params"],
                json!({"creditId":"opaque:id/1","idempotencyKey":"stable-request"})
            );
            assert_eq!(calls.last().unwrap()["method"], "account/rateLimits/read");
        }
    }

    #[test]
    #[cfg(unix)]
    fn count_only_redemption_omits_credit_id_and_failures_never_report_success_or_retry() {
        for outcome in [
            Some("reset"),
            Some("noCredit"),
            Some("nothingToReset"),
            Some("unknown"),
            None,
        ] {
            let (_fixture, mut server) = fake_server(outcome);
            assert_eq!(
                consume(&mut server, "", "stable-request").is_ok(),
                outcome == Some("reset")
            );
            let calls = calls(&server);
            let claims: Vec<_> = calls
                .iter()
                .filter(|v| v["method"] == "account/rateLimitResetCredit/consume")
                .collect();
            assert_eq!(claims.len(), 1);
            assert_eq!(
                claims[0]["params"],
                json!({"idempotencyKey":"stable-request"})
            );
        }
    }
}
