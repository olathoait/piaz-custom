//! Private pipe between the T3 Codex adapter and Piaz. Never log its response.

use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, Read, Write};
use std::process::{Command, Stdio};
use std::time::Duration;

use pitstop_core::activity::{self, verb, By, Entry};
use pitstop_core::engine::{vault_id, AccountUsage, SwitchOutcome};
use pitstop_core::provider::provider_for;
use pitstop_core::signin::{self, PrivateDir};
use pitstop_core::{vault, Engine, Host, ProviderId};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{app, duty, model, settings};

pub fn lock() -> Result<File, String> {
    let path = app::host().home().join(".pitstop/codex-operations.lock");
    let mut options = OpenOptions::new();
    options.create(true).truncate(false).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options
        .open(path)
        .map_err(|_| "Codex operation lock unavailable")?;
    file.lock()
        .map_err(|_| "Codex operation lock unavailable")?;
    Ok(file)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Request {
    #[serde(default)]
    excluded_identities: HashSet<String>,
    #[serde(default)]
    refresh: bool,
    #[serde(default)]
    identity: Option<String>,
}

fn target<H: Host>(
    engine: &Engine<H>,
    usage: &[AccountUsage],
    excluded: &HashSet<String>,
    settings: &settings::Settings,
) -> Result<String, String> {
    let state = engine
        .state()
        .map_err(|_| "Could not read enrolled accounts")?;
    let provider = provider_for(ProviderId::Codex);
    let active = provider
        .read_active(engine.host())
        .map_err(|_| "Could not read current login")?
        .ok_or("Codex is logged out")?;
    let active_identity = active
        .details
        .identity
        .ok_or("Current login has no identity")?;
    let current = state
        .accounts
        .iter()
        .find(|a| a.provider == ProviderId::Codex && a.identity.as_ref() == Some(&active_identity))
        .ok_or("Current login is not enrolled in Piaz")?;
    let mut tool = model::ToolView {
        id: "codex",
        name: "Codex",
        installed: Some(true),
        live: model::LiveView::Account {
            name: current.name.clone(),
        },
        accounts: state
            .accounts
            .iter()
            .filter(|a| a.provider == ProviderId::Codex)
            .filter(|a| a.identity.as_ref().is_some_and(|id| !excluded.contains(id)))
            .filter(|a| {
                crate::codex_warm::token_for(engine.host(), &a.name, a.name == current.name)
                    .is_some_and(|t| !t.lapsed(app::now()))
            })
            .map(|a| model::AccountView {
                name: a.name.clone(),
                label: a.label.clone(),
                plan: a.plan.clone(),
                in_use: false,
                parked_at: a.parked_at,
                usage: usage
                    .iter()
                    .find(|u| u.provider == ProviderId::Codex && u.name == a.name)
                    .map(|u| model::UsageView::from(&u.report)),
            })
            .filter(|a| match &a.usage {
                Some(model::UsageView::Ok { windows }) => {
                    !windows.is_empty()
                        && windows.iter().all(|w| {
                            w.percent.is_finite()
                                && w.percent >= 0.0
                                && w.percent < f64::from(settings.auto_switch_at)
                        })
                }
                _ => false,
            })
            .collect(),
    };
    // Another worker or the 95% switch may already have selected a usable account.
    if tool.accounts.iter().any(|a| a.name == current.name) {
        return Ok(current.name.clone());
    }
    tool.accounts.push(model::AccountView {
        name: current.name.clone(),
        label: None,
        plan: None,
        in_use: true,
        parked_at: None,
        usage: Some(model::UsageView::Ok {
            windows: vec![model::WindowView {
                label: "5h".into(),
                percent: 100.0,
                resets_at: None,
                length_secs: Some(18000),
                quiet: false,
                idle: false,
            }],
        }),
    });
    duty::switch_target(
        &tool,
        settings.auto_switch_at,
        settings.use_first,
        app::now(),
    )
    .map(|p| p.to)
    .ok_or_else(|| "No untried Codex account has usable quota".to_owned())
}

pub(crate) fn account_blob<H: Host>(host: &H, name: &str) -> Result<(Vec<u8>, bool), String> {
    let state = pitstop_core::state::State::load(host).map_err(|_| "Could not read accounts")?;
    let account = state
        .accounts
        .iter()
        .find(|a| a.provider == ProviderId::Codex && a.name == name)
        .ok_or("Account is no longer enrolled")?;
    let active = provider_for(ProviderId::Codex)
        .read_active(host)
        .map_err(|_| "Could not read login")?;
    if let Some(active) =
        active.filter(|a| a.details.identity.is_some() && a.details.identity == account.identity)
    {
        return Ok((active.blob, true));
    }
    vault::load(host, &vault_id(ProviderId::Codex, name))
        .map(|blob| (blob, false))
        .map_err(|_| "Could not read parked login".into())
}

/// Uses Codex's own refresh flow without an inference request.
fn refresh_login<H: Host>(host: &H, name: &str) -> Result<(), String> {
    let provider = provider_for(ProviderId::Codex);
    let (before, in_use) = account_blob(host, name)?;
    let identity = provider
        .details(&before)
        .identity
        .ok_or("Current login has no identity")?;
    let dir = PrivateDir::create_for(ProviderId::Codex, "warm")
        .map_err(|_| "Could not create refresh home")?;
    let auth_path = dir.path().join("auth.json");
    host.write_atomic(&auth_path, &before)
        .map_err(|_| "Could not prepare refresh")?;
    let real_home = app::real_home();
    let program =
        signin::find_program(ProviderId::Codex, &real_home).ok_or("Codex CLI is missing")?;
    let mut child = Command::new(program)
        .args(["-c", "cli_auth_credentials_store=\"file\"", "app-server"])
        .env("CODEX_HOME", dir.path())
        .env_remove("OPENAI_API_KEY")
        .env_remove("CODEX_API_KEY")
        .current_dir(dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| "Could not start Codex refresh")?;
    let output = child.stdout.take().ok_or("Refresh pipe unavailable")?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(output)
            .lines()
            .map_while(Result::ok)
        {
            if let Ok(value) = serde_json::from_str::<Value>(&line) {
                if tx.send(value).is_err() {
                    break;
                }
            }
        }
    });
    let result = (|| {
        let input = child.stdin.as_mut().ok_or("Refresh input unavailable")?;
        writeln!(input, "{}", json!({"id":1,"method":"initialize","params":{"clientInfo":{"name":"piaz-refresh","version":"1"}}})).map_err(|_| "Refresh input failed")?;
        let wait = |id| -> Result<(), String> {
            let until = std::time::Instant::now() + Duration::from_secs(30);
            loop {
                let message = rx
                    .recv_timeout(until.saturating_duration_since(std::time::Instant::now()))
                    .map_err(|_| "Codex refresh timed out")?;
                if message["id"] == id {
                    return if message.get("error").is_none() {
                        Ok(())
                    } else {
                        Err("Codex refused token refresh".into())
                    };
                }
            }
        };
        wait(1)?;
        writeln!(input, "{}", json!({"method":"initialized"}))
            .map_err(|_| "Refresh input failed")?;
        writeln!(
            input,
            "{}",
            json!({"id":2,"method":"account/read","params":{"refreshToken":true}})
        )
        .map_err(|_| "Refresh input failed")?;
        wait(2)
    })();
    let _ = child.kill();
    let _ = child.wait();
    let after = host
        .read(&auth_path)
        .map_err(|_| "Could not read refreshed login")?
        .ok_or("Refreshed login missing")?;
    if provider.details(&after).identity.as_ref() != Some(&identity) {
        return Err("Refreshed identity changed".into());
    }
    let (current, still_in_use) = account_blob(host, name)?;
    if current != before || still_in_use != in_use {
        return Err("Login changed during refresh".into());
    }
    // Persist rotated tokens even if the RPC timed out after Codex saved them.
    vault::store(host, &vault_id(ProviderId::Codex, name), &after)
        .map_err(|_| "Could not keep refreshed login")?;
    if in_use {
        provider
            .write_active(host, &after)
            .map_err(|_| "Could not activate refreshed login")?;
    }
    result
}

fn answer(request: Request) -> Result<Value, String> {
    if request.excluded_identities.len() > 32 {
        return Err("Too many excluded accounts".into());
    }
    let host = app::host();
    let settings = settings::load(&host);
    if !settings.auto_switch_codex && !request.refresh {
        return Err("Codex auto-switch is disabled in Piaz".into());
    }
    let _held = lock()?;
    let engine = Engine::new(host);
    let name = if request.refresh {
        let identity = request
            .identity
            .as_ref()
            .ok_or("Refresh identity is required")?;
        let state = engine.state().map_err(|_| "Could not read accounts")?;
        let name = state
            .accounts
            .iter()
            .find(|a| a.provider == ProviderId::Codex && a.identity.as_ref() == Some(identity))
            .ok_or("Refresh account is not enrolled")?
            .name
            .clone();
        refresh_login(engine.host(), &name)?;
        name
    } else {
        let usage = engine
            .usage(&pitstop_core::usage::Http::new())
            .map_err(|_| "Could not read fresh quota")?;
        let name = target(&engine, &usage, &request.excluded_identities, &settings)?;
        let from = app::in_use_name(ProviderId::Codex);
        match engine
            .switch_keeping_unmanaged(&name, Some(ProviderId::Codex))
            .map_err(|_| "Could not switch Codex")?
        {
            SwitchOutcome::WouldReplace(_) => {
                return Err("Refusing to replace an unmanaged login".into())
            }
            SwitchOutcome::Switched { .. } => {
                let _ = activity::record(
                    engine.host(),
                    &Entry::new(app::now(), verb::AUTO_SWITCH, By::Cli)
                        .provider(ProviderId::Codex)
                        .account(&name)
                        .from(from.as_deref())
                        .detail("t3-limit"),
                );
            }
            SwitchOutcome::AlreadyActive => {}
        }
        name
    };
    let (blob, _) = account_blob(engine.host(), &name)?;
    let identity = provider_for(ProviderId::Codex)
        .details(&blob)
        .identity
        .ok_or("Selected login has no identity")?;
    if !request.refresh && request.excluded_identities.contains(&identity) {
        return Err("Selected account was already exhausted".into());
    }
    let auth: Value = serde_json::from_slice(&blob).map_err(|_| "Invalid selected login")?;
    let token = auth["tokens"]["access_token"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or("Missing access token")?;
    let account_id = auth["tokens"]["account_id"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or("Missing account id")?;
    Ok(json!({"ok":true,"identity":identity,"name":name,"auth":{
        "type":"chatgptAuthTokens","accessToken":token,"chatgptAccountId":account_id
    }}))
}

pub fn headless() -> i32 {
    let result = (|| {
        let mut input = String::new();
        std::io::stdin()
            .take(16385)
            .read_to_string(&mut input)
            .map_err(|_| "Could not read bridge request")?;
        if input.len() > 16384 {
            return Err("Bridge request too large".into());
        }
        let request = serde_json::from_str(&input).map_err(|_| "Invalid bridge request")?;
        answer(request)
    })();
    let (code, output) = match result {
        Ok(output) => (0, output),
        Err(reason) => (1, json!({"ok":false,"reason":reason})),
    };
    // stdout is a private child-process pipe, not the application's log.
    let _ = writeln!(std::io::stdout().lock(), "{output}");
    code
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
    use pitstop_core::testing::FakeHost;
    use pitstop_core::usage::{Usage, UsageReport, Window};

    fn enrolled() -> Engine<FakeHost> {
        let engine = Engine::new(FakeHost::new());
        let provider = provider_for(ProviderId::Codex);
        for user in ["a", "b", "c"] {
            let claims = URL_SAFE_NO_PAD.encode(json!({"sub": user}).to_string());
            let blob = serde_json::to_vec(&json!({"tokens": {
                "id_token": format!("header.{claims}.sig"), "account_id": "personal",
                "access_token": "TEST-ACCESS", "refresh_token": "TEST-REFRESH"
            }}))
            .unwrap();
            provider.write_active(engine.host(), &blob).unwrap();
            engine.enroll(ProviderId::Codex, user).unwrap();
        }
        engine.switch("a", Some(ProviderId::Codex)).unwrap();
        engine
    }
    fn reading(name: &str, five: f64, week: f64) -> AccountUsage {
        AccountUsage {
            provider: ProviderId::Codex,
            name: name.into(),
            report: UsageReport::Ok(Usage {
                plan: None,
                windows: [("5h", five), ("week", week)]
                    .into_iter()
                    .map(|(label, percent)| Window {
                        label: label.into(),
                        percent,
                        resets_at: Some(app::now() + 1000),
                        length_secs: Some(18000),
                        quiet: false,
                        idle: false,
                    })
                    .collect(),
            }),
        }
    }
    #[test]
    fn recovery_skips_exhausted_and_previously_tried_accounts() {
        let engine = enrolled();
        let usage = [
            reading("a", 100.0, 20.0),
            reading("b", 0.0, 100.0),
            reading("c", 20.0, 30.0),
        ];
        let excluded = ["a:personal".to_string()].into();
        let settings = settings::Settings {
            auto_switch_codex: true,
            auto_switch_at: 95,
            ..Default::default()
        };
        assert_eq!(target(&engine, &usage, &excluded, &settings).unwrap(), "c");
        let excluded = ["a:personal".to_string(), "c:personal".to_string()].into();
        assert!(target(&engine, &usage, &excluded, &settings).is_err());
    }
    #[test]
    fn concurrent_recovery_reuses_the_already_switched_healthy_account() {
        let engine = enrolled();
        engine.switch("c", Some(ProviderId::Codex)).unwrap();
        let usage = [
            reading("a", 100.0, 20.0),
            reading("b", 0.0, 20.0),
            reading("c", 20.0, 30.0),
        ];
        let excluded = ["a:personal".to_string()].into();
        assert_eq!(
            target(&engine, &usage, &excluded, &settings::Settings::default()).unwrap(),
            "c"
        );
    }
    #[test]
    fn failed_quota_is_not_a_candidate_and_parked_refresh_does_not_read_active_auth() {
        let engine = enrolled();
        let mut usage = vec![reading("a", 100.0, 20.0), reading("b", 0.0, 20.0)];
        usage[1].report = UsageReport::Failed("network".into());
        assert!(target(
            &engine,
            &usage,
            &["a:personal".into()].into(),
            &settings::Settings::default()
        )
        .is_err());
        assert!(account_blob(engine.host(), "a").unwrap().1);
        assert!(!account_blob(engine.host(), "b").unwrap().1);
    }
    #[test]
    fn bridge_request_rejects_unexpected_fields() {
        assert!(serde_json::from_str::<Request>(r#"{"arbitraryCommand":"rm"}"#).is_err());
        assert!(
            serde_json::from_str::<Request>(r#"{"excludedIdentities":[],"refresh":false}"#).is_ok()
        );
    }
}
