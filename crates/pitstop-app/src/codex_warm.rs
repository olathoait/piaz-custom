//! Codex warm-ups use a private home, never the project's config or live login.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use pitstop_core::engine::vault_id;
use pitstop_core::provider::provider_for;
use pitstop_core::signin::PrivateDir;
use pitstop_core::{vault, Host, ProviderId};
use serde_json::Value;

use crate::warm::{self, Failure, Token};

fn login(host: &dyn Host, name: &str, in_use: bool) -> Option<Vec<u8>> {
    if in_use {
        Some(
            provider_for(ProviderId::Codex)
                .read_active(host)
                .ok()??
                .blob,
        )
    } else {
        vault::load(host, &vault_id(ProviderId::Codex, name)).ok()
    }
}

pub fn token_for(host: &dyn Host, name: &str, in_use: bool) -> Option<Token> {
    let auth: Value = serde_json::from_slice(&login(host, name, in_use)?).ok()?;
    let tokens = &auth["tokens"];
    let access = tokens["access_token"].as_str()?.to_owned();
    if access.is_empty() {
        return None;
    }
    let expires_at = access
        .split('.')
        .nth(1)
        .and_then(|p| URL_SAFE_NO_PAD.decode(p).ok())
        .and_then(|p| serde_json::from_slice::<Value>(&p).ok())
        .and_then(|p| p["exp"].as_i64());
    Some(Token {
        access,
        expires_at,
        renewable: tokens["refresh_token"]
            .as_str()
            .is_some_and(|s| !s.is_empty()),
    })
}

pub fn command(program: &Path, dir: &Path, search: &[PathBuf]) -> Command {
    let mut command = Command::new(program);
    command
        .args([
            "exec",
            "--ephemeral",
            "--ignore-user-config",
            "--ignore-rules",
            "--skip-git-repo-check",
            "--sandbox",
            "read-only",
            "--json",
            "--disable",
            "shell_tool",
            "--disable",
            "unified_exec",
            "--disable",
            "apps",
            "--disable",
            "multi_agent",
            "-c",
            "cli_auth_credentials_store=\"file\"",
            "-c",
            "web_search=\"disabled\"",
            "-c",
            "model_reasoning_effort=\"low\"",
            "Reply with the single word OK. Do not use tools.",
        ])
        .current_dir(dir)
        .env("CODEX_HOME", dir)
        .env_remove("OPENAI_API_KEY")
        .env_remove("CODEX_API_KEY")
        .env_remove("OPENAI_BASE_URL")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if let Ok(path) = std::env::join_paths(search) {
        command.env("PATH", path);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    command
}

pub fn verdict(exit: Option<i32>, stdout: &str) -> Result<(), Failure> {
    let mut answered = false;
    let mut completed = false;
    let mut failure = None;
    for event in stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
    {
        match event["type"].as_str() {
            Some("item.completed") if event["item"]["type"] == "agent_message" => {
                answered |= event["item"]["text"]
                    .as_str()
                    .is_some_and(|s| s.trim() == "OK");
            }
            Some("turn.completed") => completed = true,
            Some("turn.failed" | "error") => {
                let message = event["error"]["message"]
                    .as_str()
                    .or_else(|| event["message"].as_str())
                    .unwrap_or("")
                    .to_lowercase();
                failure = Some(
                    if message.contains("usage_limit_reached") || message.contains("usage limit") {
                        Failure::LimitReached
                    } else if message.contains("401")
                        || message.contains("403")
                        || message.contains("refresh token")
                        || message.contains("unauthorized")
                    {
                        Failure::LoginRefused
                    } else if [
                        "429",
                        "rate limit",
                        "500",
                        "502",
                        "503",
                        "504",
                        "connection",
                        "network",
                        "timed out",
                    ]
                    .iter()
                    .any(|s| message.contains(s))
                    {
                        Failure::Unreachable
                    } else {
                        // Provider output may contain credentials; never persist its raw error.
                        Failure::Failed("Codex warm-up failed".into())
                    },
                );
            }
            _ => {}
        }
    }
    if exit == Some(0) && completed && answered && failure.is_none() {
        Ok(())
    } else {
        Err(failure.unwrap_or(Failure::Failed(
            "Codex returned no completed OK response".into(),
        )))
    }
}

fn private_login(blob: &[u8], in_use: bool) -> Result<Vec<u8>, Failure> {
    let mut auth: Value = serde_json::from_slice(blob).map_err(|_| Failure::LoginRefused)?;
    if !auth["tokens"]["access_token"]
        .as_str()
        .is_some_and(|t| !t.is_empty())
    {
        return Err(Failure::LoginRefused);
    }
    auth["OPENAI_API_KEY"] = Value::Null;
    if in_use {
        // The running provider owns token renewal; a warm-up must not rotate its refresh token.
        auth["tokens"]["refresh_token"] = Value::String(String::new());
    }
    serde_json::to_vec(&auth).map_err(|_| Failure::LoginRefused)
}

pub fn run(
    host: &dyn Host,
    name: &str,
    in_use: bool,
    program: &Path,
    search: &[PathBuf],
) -> Result<(), Failure> {
    let blob = login(host, name, in_use).ok_or(Failure::LoginRefused)?;
    let auth = private_login(&blob, in_use)?;
    let dir = PrivateDir::create_for(ProviderId::Codex, "warm")
        .map_err(|_| Failure::Failed("Couldn't create private Codex home".into()))?;
    let path = dir.path().join("auth.json");
    host.write_atomic(&path, &auth)
        .map_err(|_| Failure::Failed("Couldn't lend Codex login".into()))?;
    let outcome = warm::run_command(command(program, dir.path(), search), verdict);
    if !in_use {
        let renewed = host
            .read(&path)
            .map_err(|_| Failure::Failed("Couldn't read renewed Codex login".into()))?
            .ok_or(Failure::LoginRefused)?;
        keep_renewed(host, name, &blob, &renewed)?;
    }
    outcome
}

fn keep_renewed(host: &dyn Host, name: &str, before: &[u8], after: &[u8]) -> Result<(), Failure> {
    if before == after {
        return Ok(());
    }
    let provider = provider_for(ProviderId::Codex);
    let identity = provider
        .details(before)
        .identity
        .ok_or(Failure::LoginRefused)?;
    if provider.details(after).identity.as_ref() != Some(&identity) {
        return Err(Failure::LoginRefused);
    }
    let id = vault_id(ProviderId::Codex, name);
    if vault::load(host, &id).ok().as_deref() != Some(before) {
        return Err(Failure::Failed("Codex login changed during warm-up".into()));
    }
    vault::store(host, &id, after)
        .map_err(|_| Failure::Failed("Couldn't keep renewed Codex login".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renewed_login_keeps_identity_and_never_overwrites_a_concurrent_change() {
        let host = pitstop_core::testing::FakeHost::new();
        let credential = |user: &str, access: &str| {
            let claims = URL_SAFE_NO_PAD.encode(serde_json::json!({"sub": user}).to_string());
            serde_json::to_vec(&serde_json::json!({"tokens": {
                "id_token": format!("header.{claims}.signature"), "account_id": "personal", "access_token": access,
            }})).unwrap()
        };
        let before = credential("one", "old");
        let renewed = credential("one", "new");
        let id = vault_id(ProviderId::Codex, "one");
        vault::store(&host, &id, &before).unwrap();
        keep_renewed(&host, "one", &before, &renewed).unwrap();
        assert!(vault::load(&host, &id).unwrap() == renewed);
        assert!(keep_renewed(&host, "one", &renewed, &credential("other", "new")).is_err());
        assert!(keep_renewed(&host, "one", &before, &credential("one", "stale")).is_err());
        assert!(vault::load(&host, &id).unwrap() == renewed);
    }

    #[test]
    fn active_login_cannot_refresh_and_no_warmup_can_fall_back_to_api_billing() {
        let blob = br#"{"OPENAI_API_KEY":"SECRET","tokens":{"access_token":"access","refresh_token":"refresh"}}"#;
        let active: Value = serde_json::from_slice(&private_login(blob, true).unwrap()).unwrap();
        assert_eq!(active["tokens"]["refresh_token"], "");
        assert!(active["OPENAI_API_KEY"].is_null());
        let parked: Value = serde_json::from_slice(&private_login(blob, false).unwrap()).unwrap();
        assert_eq!(parked["tokens"]["refresh_token"], "refresh");
        assert!(parked["OPENAI_API_KEY"].is_null());
        assert!(private_login(br#"{"OPENAI_API_KEY":"SECRET"}"#, false).is_err());
    }

    #[test]
    #[ignore = "Uses a single real Codex inference request; run explicitly"]
    fn real_codex_warmup_keeps_active_login() {
        use pitstop_core::{Engine, RealHost};
        let host = RealHost::default();
        let engine = Engine::new(RealHost::default());
        let pitstop_core::live::Live::Account(name) = engine.live(ProviderId::Codex).unwrap()
        else {
            panic!("Codex must be enrolled before the smoke test");
        };
        let provider = provider_for(ProviderId::Codex);
        let before = provider.read_active(&host).unwrap().unwrap().blob;
        let home = host.home();
        let program = pitstop_core::signin::find_program(ProviderId::Codex, &home).unwrap();
        let result = run(
            &host,
            &name,
            true,
            &program,
            &pitstop_core::signin::search_path(&home),
        );
        let after = provider.read_active(&host).unwrap().unwrap().blob;
        assert!(
            before == after,
            "Warm-up must not change the live Codex login"
        );
        assert!(result.is_ok(), "Codex warm-up failed: {result:?}");
    }

    #[test]
    fn completed_message_and_turn_are_both_required() {
        let ok = "{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"OK\"}}\n{\"type\":\"turn.completed\"}";
        assert_eq!(verdict(Some(0), ok), Ok(()));
        assert!(verdict(Some(1), ok).is_err());
        assert!(verdict(Some(0), "{\"type\":\"turn.completed\"}").is_err());
        assert!(verdict(Some(0), "not JSON").is_err());
    }

    #[test]
    fn errors_distinguish_quota_auth_and_transient_failures_without_leaking_output() {
        for (message, expected) in [
            ("You have hit your usage limit", Failure::LimitReached),
            ("401 unauthorized SECRET", Failure::LoginRefused),
            ("429 too many requests", Failure::Unreachable),
            ("503 connection error", Failure::Unreachable),
            ("SECRET", Failure::Failed("Codex warm-up failed".into())),
        ] {
            let event = serde_json::json!({"type":"turn.failed", "error":{"message":message}});
            assert_eq!(verdict(Some(1), &event.to_string()), Err(expected));
        }
    }

    #[test]
    fn command_is_isolated_ephemeral_and_read_only() {
        let cmd = command(Path::new("codex"), Path::new("/private/warm"), &[]);
        let args: Vec<_> = cmd.get_args().map(|s| s.to_string_lossy()).collect();
        assert!(args.iter().any(|s| s == "--ephemeral"));
        assert!(args.iter().any(|s| s == "--ignore-user-config"));
        assert!(args.iter().any(|s| s == "read-only"));
        assert!(!args.iter().any(|s| s.contains("bypass")));
        assert_eq!(cmd.get_current_dir(), Some(Path::new("/private/warm")));
    }
}
