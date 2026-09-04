use crate::{
    browser_provider_runtime, browser_session_runtime, config::AppConfig, quota_usage_runtime,
};
use serde::Serialize;
use std::env;

#[derive(Clone, Debug, Serialize)]
pub struct AccountReadiness {
    pub account_id: String,
    pub provider: String,
    pub transport: String,
    pub effective_status: String,
    pub routable: bool,
    pub reasons: Vec<String>,
    pub credential_configured: Option<bool>,
    pub browser_ready: Option<bool>,
    pub browser_session_id: Option<String>,
    pub browser_session_status: Option<String>,
    pub browser_last_error: Option<String>,
    pub browser_adapter_status: Option<String>,
    pub browser_adapter_message: Option<String>,
    pub quota_blocked: bool,
    pub quota_pressure: f64,
    pub route_count: usize,
    pub healthy_route_count: usize,
    pub cooling_route_count: usize,
}

impl AccountReadiness {
    fn unavailable(account_id: &str, reason: &str) -> Self {
        Self {
            account_id: account_id.to_string(),
            provider: String::new(),
            transport: "unknown".into(),
            effective_status: "unavailable".into(),
            routable: false,
            reasons: vec![reason.into()],
            credential_configured: None,
            browser_ready: None,
            browser_session_id: None,
            browser_session_status: None,
            browser_last_error: None,
            browser_adapter_status: None,
            browser_adapter_message: None,
            quota_blocked: false,
            quota_pressure: 0.0,
            route_count: 0,
            healthy_route_count: 0,
            cooling_route_count: 0,
        }
    }

    pub fn add_soft_reason(&mut self, reason: &str) {
        if !self.reasons.iter().any(|item| item == reason) {
            self.reasons.push(reason.to_string());
        }
        if self.routable && self.effective_status == "ready" {
            self.effective_status = "degraded".into();
        }
    }
}

pub async fn evaluate_base(config: &AppConfig, account_id: &str) -> AccountReadiness {
    let Some(account) = config.account(account_id) else {
        return AccountReadiness::unavailable(account_id, "unknown_account");
    };
    let Some(provider) = config.provider(&account.provider) else {
        return AccountReadiness::unavailable(account_id, "unknown_provider");
    };

    let mut readiness = AccountReadiness {
        account_id: account.id.clone(),
        provider: provider.id.clone(),
        transport: provider.transport().into(),
        effective_status: "ready".into(),
        routable: true,
        reasons: Vec::new(),
        credential_configured: None,
        browser_ready: None,
        browser_session_id: None,
        browser_session_status: None,
        browser_last_error: None,
        browser_adapter_status: None,
        browser_adapter_message: None,
        quota_blocked: false,
        quota_pressure: 0.0,
        route_count: 0,
        healthy_route_count: 0,
        cooling_route_count: 0,
    };

    if !account.enabled {
        readiness.effective_status = "unavailable".into();
        readiness.routable = false;
        readiness.reasons.push("account_disabled".into());
        return readiness;
    }

    if provider.is_browser() {
        let browser_ready = match browser_provider_runtime::get() {
            Some(registry) => {
                if let Some(session_id) = registry.session_id_for_account(&account.id) {
                    readiness.browser_session_id = Some(session_id.to_string());
                    if let Some(store) = browser_session_runtime::get() {
                        if let Ok(session) = store.session(&session_id).await {
                            readiness.browser_session_status = Some(session.status.clone());
                            readiness.browser_last_error = session.last_error.clone();
                        }
                    }
                }
                let available = registry.route_available(&provider.kind, &account.id).await;
                let diagnostics = registry
                    .adapter_diagnostics(&provider.kind, &account.id)
                    .await;
                readiness.browser_adapter_status = Some(diagnostics.status);
                readiness.browser_adapter_message = Some(diagnostics.message);
                available
            }
            None => false,
        };
        readiness.browser_ready = Some(browser_ready);
        if !browser_ready {
            readiness.effective_status = "unavailable".into();
            readiness.routable = false;
            let reason = match readiness.browser_adapter_status.as_deref() {
                Some("adapter_incompatible") => "browser_adapter_incompatible",
                Some("login_required") => "browser_adapter_login_required",
                Some("unsupported") | Some("unconfigured") => "browser_adapter_unavailable",
                _ => match readiness.browser_session_status.as_deref() {
                    Some("stopped") => "browser_session_stopped",
                    Some("login_required") | Some("starting") => "browser_login_required",
                    Some("degraded") => "browser_session_degraded",
                    Some("requires_attention") => "browser_session_requires_attention",
                    Some("failed") => "browser_session_failed",
                    _ => "browser_session_not_ready",
                },
            };
            readiness.reasons.push(reason.into());
        }
    } else {
        let configured = env::var(&account.api_key_env)
            .map(|value| !value.trim().is_empty())
            .unwrap_or(false);
        readiness.credential_configured = Some(configured);
        if !configured {
            readiness.effective_status = "unavailable".into();
            readiness.routable = false;
            readiness.reasons.push("credential_missing".into());
        }
    }

    if let Some(usage) = quota_usage_runtime::get() {
        match usage.account_snapshot(account_id).await {
            Ok(snapshot) => {
                readiness.quota_blocked = snapshot.blocked;
                readiness.quota_pressure = snapshot.daily.pressure.max(snapshot.monthly.pressure);
                if snapshot.blocked {
                    readiness.effective_status = "unavailable".into();
                    readiness.routable = false;
                    readiness.reasons.push("quota_blocked".into());
                } else if readiness.quota_pressure >= 0.8
                    || snapshot.remaining_requests_hint == Some(0)
                    || snapshot.remaining_tokens_hint == Some(0)
                    || snapshot.consecutive_429 > 0
                {
                    readiness.add_soft_reason("quota_pressure");
                }
            }
            Err(_) => readiness.add_soft_reason("quota_state_unavailable"),
        }
    }

    readiness
}

#[cfg(test)]
mod tests {
    use super::AccountReadiness;

    #[test]
    fn soft_reason_degrades_only_routable_accounts() {
        let mut readiness = AccountReadiness {
            account_id: "a".into(),
            provider: "p".into(),
            transport: "api".into(),
            effective_status: "ready".into(),
            routable: true,
            reasons: Vec::new(),
            credential_configured: Some(true),
            browser_ready: None,
            browser_session_id: None,
            browser_session_status: None,
            browser_last_error: None,
            browser_adapter_status: None,
            browser_adapter_message: None,
            quota_blocked: false,
            quota_pressure: 0.0,
            route_count: 0,
            healthy_route_count: 0,
            cooling_route_count: 0,
        };
        readiness.add_soft_reason("route_cooldown");
        readiness.add_soft_reason("route_cooldown");
        assert_eq!(readiness.effective_status, "degraded");
        assert_eq!(readiness.reasons, vec!["route_cooldown"]);
    }
}
