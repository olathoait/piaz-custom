//! Claude's limited-use reset grants, requested explicitly by the account owner.
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::engine::vault_id;
use crate::error::{Error, Result};
use crate::provider::provider_for;
use crate::usage::{self, FetchError, Token};
use crate::{vault, Engine, Host, ProviderId};

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(rename_all(serialize = "camelCase", deserialize = "snake_case"))]
pub struct Status {
    #[serde(skip_deserializing)]
    pub identity: String,
    #[serde(skip_deserializing)]
    pub available_count: Option<u32>,
    pub eligible: bool,
    #[serde(default)]
    pub grants: Vec<Grant>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all(serialize = "camelCase", deserialize = "snake_case"))]
pub struct Grant {
    pub id: String,
    #[serde(default)]
    pub label: String,
    pub resets_left: u32,
    #[serde(default)]
    pub ends_at: Option<String>,
    #[serde(default)]
    pub clears: Vec<String>,
    #[serde(default)]
    pub usable_now: bool,
    #[serde(default)]
    pub paused: bool,
}

impl Status {
    pub fn parse(body: Value) -> Result<Self> {
        if !body.is_object() {
            return Err(invalid("Unreadable limit reset status"));
        }
        match body.get("cedar_ember").filter(|v| !v.is_null()) {
            None if body.get("five_hour").is_some() || body.get("cedar_ember").is_some() => {
                Ok(Self::default())
            }
            Some(status) => serde_json::from_value(status.clone())
                .map_err(|_| invalid("Unreadable limit reset status")),
            _ => Err(invalid("Unreadable limit reset status")),
        }
    }
}

/// Kept separate from regular usage reads: checking usage never spends a reset.
pub trait ResetApi {
    fn profile(&self, token: &str) -> std::result::Result<Value, FetchError>;
    fn grants(&self, token: &str) -> std::result::Result<Value, FetchError>;
    fn claim(
        &self,
        token: &str,
        organization: &str,
        grant: &str,
        request: &str,
    ) -> std::result::Result<Value, FetchError>;
}

fn invalid(message: &str) -> Error {
    Error::InvalidData(message.into())
}

fn fetched(error: FetchError) -> Error {
    invalid(match error {
        FetchError::Unauthorized => "Claude refused this login; sign in again",
        FetchError::Other(_) => {
            "Could not confirm the reset request. Check usage before trying again"
        }
    })
}

fn identifier(value: &str, max: usize) -> bool {
    !value.is_empty()
        && value.len() <= max
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
}

impl<H: Host> Engine<H> {
    fn reset_login(&self, name: &str, api: &dyn ResetApi) -> Result<(String, String, String)> {
        let pid = ProviderId::ClaudeCode;
        let state = self.state()?;
        let account = state
            .accounts
            .iter()
            .find(|a| a.provider == pid && a.name == name)
            .ok_or_else(|| Error::UnknownAccount(name.into()))?;
        let active = provider_for(pid).read_active(self.host())?;
        let blob = match active
            .filter(|a| a.details.identity.is_some() && a.details.identity == account.identity)
        {
            Some(active) => active.blob,
            None => vault::load(self.host(), &vault_id(pid, name))?,
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        let Token::Valid(token) = usage::claude_token(&blob, now) else {
            return Err(invalid(
                "This login is expired or signed out; renew it before checking resets",
            ));
        };
        // A fresh profile proves whose scarce reset would be consumed.
        let profile = api.profile(&token).map_err(fetched)?;
        let (identity, _) = usage::profile_owner(&profile)
            .ok_or_else(|| invalid("Could not verify this Claude account"))?;
        if account.identity.as_ref() != Some(&identity) {
            return Err(invalid(
                "The Claude token belongs to another account; sign in again",
            ));
        }
        let organization = profile
            .pointer("/organization/uuid")
            .and_then(Value::as_str)
            .filter(|id| identifier(id, 64))
            .ok_or_else(|| invalid("Claude organization is missing or invalid"))?;
        Ok((token, organization.to_owned(), identity))
    }

    pub fn limit_resets(&self, name: &str, api: &dyn ResetApi) -> Result<Status> {
        let (token, _, identity) = self.reset_login(name, api)?;
        let mut status = Status::parse(api.grants(&token).map_err(fetched)?)?;
        status.identity = identity;
        Ok(status)
    }

    pub fn use_limit_reset(
        &self,
        name: &str,
        identity: &str,
        grant: &str,
        request: &str,
        api: &dyn ResetApi,
    ) -> Result<()> {
        if !identifier(grant, 40) || !identifier(request, 64) {
            return Err(invalid("Invalid reset grant or request id"));
        }
        let (token, organization, current_identity) = self.reset_login(name, api)?;
        if identity != current_identity {
            return Err(invalid("The selected account changed; check resets again"));
        }
        let status = Status::parse(api.grants(&token).map_err(fetched)?)?;
        if !status.eligible
            || !status
                .grants
                .iter()
                .any(|g| g.id == grant && g.resets_left > 0 && g.usable_now && !g.paused)
        {
            return Err(invalid(
                "This limit reset is no longer available; check again",
            ));
        }
        // One POST only: a lost response may already have consumed this grant.
        let answer = api
            .claim(&token, &organization, grant, request)
            .map_err(fetched)?;
        match answer.get("result").and_then(Value::as_str) {
            Some("reset") => Ok(()),
            Some("already_used" | "not_limited" | "cooldown" | "ineligible" | "unavailable") => {
                Err(invalid(
                    "Claude did not apply a reset; check availability and usage again",
                ))
            }
            _ => Err(invalid(
                "Could not confirm the reset. Check usage before trying again",
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::FakeHost;
    use serde_json::json;
    use std::cell::{Cell, RefCell};

    struct Api {
        status: Value,
        reply: Option<Value>,
        wrong_owner: bool,
        claims: RefCell<Vec<(String, String, String, String)>>,
        reads: Cell<usize>,
    }

    impl Api {
        fn new() -> Self {
            Self {
                status: json!({"cedar_ember":{"eligible":true,"grants":[{
                    "id":"weekly-1", "label":"Weekly reset", "resets_left":2,
                    "ends_at":"2099-01-01T00:00:00Z", "clears":["five_hour","seven_day"],
                    "usable_now":true, "paused":false
                }]}}),
                reply: Some(json!({"result":"reset"})),
                wrong_owner: false,
                claims: RefCell::new(Vec::new()),
                reads: Cell::new(0),
            }
        }
    }

    impl ResetApi for Api {
        fn profile(&self, token: &str) -> std::result::Result<Value, FetchError> {
            let user = if self.wrong_owner { "other" } else { token };
            Ok(json!({"account":{"uuid":user},"organization":{"uuid":"org-1"}}))
        }
        fn grants(&self, _: &str) -> std::result::Result<Value, FetchError> {
            self.reads.set(self.reads.get() + 1);
            Ok(self.status.clone())
        }
        fn claim(
            &self,
            token: &str,
            org: &str,
            grant: &str,
            request: &str,
        ) -> std::result::Result<Value, FetchError> {
            self.claims
                .borrow_mut()
                .push((token.into(), org.into(), grant.into(), request.into()));
            self.reply
                .clone()
                .ok_or_else(|| FetchError::Other("lost response".into()))
        }
    }

    fn enrolled() -> Engine<FakeHost> {
        let engine = Engine::new(FakeHost::new());
        for user in ["a", "b"] {
            let credentials =
                json!({"claudeAiOauth":{"accessToken":user,"expiresAt":9999999999999i64}})
                    .to_string();
            let blob = serde_json::to_vec(&json!({"credentials":credentials,
                "oauth_account":{"accountUuid":user,"organizationUuid":"org-1"}
            }))
            .unwrap();
            provider_for(ProviderId::ClaudeCode)
                .write_active(engine.host(), &blob)
                .unwrap();
            engine.enroll(ProviderId::ClaudeCode, user).unwrap();
        }
        engine
    }

    #[test]
    fn checking_active_and_parked_accounts_does_not_spend_or_switch() {
        let engine = enrolled();
        let api = Api::new();
        let before = provider_for(ProviderId::ClaudeCode)
            .read_active(engine.host())
            .unwrap()
            .unwrap()
            .blob;
        for name in ["a", "b"] {
            let status = engine.limit_resets(name, &api).unwrap();
            assert_eq!(status.grants[0].resets_left, 2);
            let serialized = serde_json::to_value(status).unwrap();
            assert_eq!(serialized["grants"][0]["usableNow"], true);
        }
        assert!(api.claims.borrow().is_empty());
        assert_eq!(
            provider_for(ProviderId::ClaudeCode)
                .read_active(engine.host())
                .unwrap()
                .unwrap()
                .blob,
            before
        );
    }

    #[test]
    fn claiming_rechecks_availability_and_uses_the_selected_accounts_org_and_request_id() {
        let engine = enrolled();
        let api = Api::new();
        engine
            .use_limit_reset("a", "a:org-1", "weekly-1", "request-1", &api)
            .unwrap();
        assert_eq!(api.reads.get(), 1);
        assert_eq!(
            *api.claims.borrow(),
            vec![(
                "a".into(),
                "org-1".into(),
                "weekly-1".into(),
                "request-1".into()
            )]
        );
    }

    #[test]
    fn stale_ineligible_paused_exhausted_and_unknown_grants_never_post() {
        let engine = enrolled();
        for (pointer, value) in [
            ("/cedar_ember/eligible", json!(false)),
            ("/cedar_ember/grants/0/usable_now", json!(false)),
            ("/cedar_ember/grants/0/paused", json!(true)),
            ("/cedar_ember/grants/0/resets_left", json!(0)),
            ("/cedar_ember/grants/0/id", json!("other")),
        ] {
            let mut api = Api::new();
            *api.status.pointer_mut(pointer).unwrap() = value;
            assert!(engine
                .use_limit_reset("a", "a:org-1", "weekly-1", "request-1", &api)
                .is_err());
            assert!(api.claims.borrow().is_empty());
        }
    }

    #[test]
    fn wrong_owner_unknown_account_and_bad_ids_never_post() {
        let engine = enrolled();
        let mut api = Api::new();
        api.wrong_owner = true;
        assert!(engine.limit_resets("a", &api).is_err());
        assert!(engine
            .use_limit_reset("a", "a:org-1", "weekly-1", "request-1", &api)
            .is_err());
        assert!(engine
            .use_limit_reset("unknown", "a:org-1", "weekly-1", "request-1", &api)
            .is_err());
        for bad in ["", "../x", "x?y", "a b"] {
            assert!(engine
                .use_limit_reset("a", "a:org-1", bad, "request-1", &api)
                .is_err());
            assert!(engine
                .use_limit_reset("a", "a:org-1", "weekly-1", bad, &api)
                .is_err());
        }
        assert_eq!(api.reads.get(), 0);
        assert!(api.claims.borrow().is_empty());
    }

    #[test]
    fn an_offer_cannot_be_used_after_the_selected_identity_changes() {
        let engine = enrolled();
        let api = Api::new();
        assert!(engine
            .use_limit_reset("a", "b:org-1", "weekly-1", "request-1", &api)
            .is_err());
        assert_eq!(api.reads.get(), 0);
        assert!(api.claims.borrow().is_empty());
    }

    #[test]
    fn expired_credentials_never_send_a_reset_request() {
        let engine = enrolled();
        let api = Api::new();
        let provider = provider_for(ProviderId::ClaudeCode);
        let active = provider.read_active(engine.host()).unwrap().unwrap();
        let mut blob: Value = serde_json::from_slice(&active.blob).unwrap();
        let mut credentials: Value =
            serde_json::from_str(blob["credentials"].as_str().unwrap()).unwrap();
        credentials["claudeAiOauth"]["expiresAt"] = json!(1);
        blob["credentials"] = json!(credentials.to_string());
        provider
            .write_active(engine.host(), &serde_json::to_vec(&blob).unwrap())
            .unwrap();
        assert!(engine.limit_resets("b", &api).is_err());
        assert!(engine
            .use_limit_reset("b", "b:org-1", "weekly-1", "request-1", &api)
            .is_err());
        assert_eq!(api.reads.get(), 0);
        assert!(api.claims.borrow().is_empty());
    }

    #[test]
    fn lost_or_negative_responses_never_retry_or_report_success() {
        let engine = enrolled();
        for reply in [
            None,
            Some(json!({})),
            Some(json!({"result":"already_used"})),
            Some(json!({"result":"not_limited"})),
            Some(json!({"result":"cooldown"})),
        ] {
            let mut api = Api::new();
            api.reply = reply;
            assert!(engine
                .use_limit_reset("a", "a:org-1", "weekly-1", "request-1", &api)
                .is_err());
            assert_eq!(api.claims.borrow().len(), 1);
        }
    }

    #[test]
    fn absent_offers_are_empty_but_malformed_answers_are_errors() {
        for body in [json!({"cedar_ember":null}), json!({"five_hour":null})] {
            assert!(Status::parse(body).unwrap().grants.is_empty());
        }
        for body in [
            json!({}),
            json!(null),
            json!({"error":"refused"}),
            json!({"cedar_ember":{"eligible":true,"grants":[{"id":"x"}]}}),
        ] {
            assert!(Status::parse(body).is_err());
        }
    }
}
