use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use serde::Deserialize;

use super::wire::*;
use crate::auth::error::RefreshTokenFailedReason;
use crate::auth::manager::RefreshReason;
use crate::auth::model::{AuthMode, GrokAuth};
use crate::auth::providers::SubscriptionProvider;
use crate::auth::refresh::{AuthSnapshot, RefreshOutcome, TokenRefresher};

#[derive(Deserialize)]
struct AnthropicRefreshResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
}

pub struct AnthropicRefresher {
    auth: Arc<dyn AuthSnapshot>,
}

use crate::auth::manager::AuthManager;

impl AnthropicRefresher {
    pub(crate) fn new(auth: Arc<dyn AuthSnapshot>) -> Self {
        Self { auth }
    }

    pub fn for_manager(auth_manager: Arc<AuthManager>) -> Self {
        Self { auth: auth_manager }
    }

    pub async fn refresh_credential(&self) -> anyhow::Result<GrokAuth> {
        match self.refresh(RefreshReason::PreRequest).await {
            RefreshOutcome::Success(auth) => Ok(*auth),
            RefreshOutcome::PermanentFailure { error, .. } => {
                anyhow::bail!("Refresh failed: {error}")
            }
            RefreshOutcome::TransientFailure { message, .. } => {
                anyhow::bail!("Refresh failed: {message}")
            }
        }
    }
}

#[async_trait::async_trait]
impl TokenRefresher for AnthropicRefresher {
    async fn refresh(&self, _reason: RefreshReason) -> RefreshOutcome {
        let current_auth = match self
            .auth
            .expired_auth()
            .or_else(|| self.auth.read_disk_auth())
        {
            Some(a) => a,
            None => {
                return RefreshOutcome::permanent(
                    RefreshTokenFailedReason::RefreshTokenRejected,
                    None,
                );
            }
        };

        let current_rt = match current_auth.refresh_token.as_deref() {
            Some(rt) if !rt.trim().is_empty() => rt.to_owned(),
            _ => {
                return RefreshOutcome::permanent(
                    RefreshTokenFailedReason::RefreshTokenRejected,
                    Some(current_auth.key),
                );
            }
        };

        let client = match reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
        {
            Ok(c) => c,
            Err(e) => {
                return RefreshOutcome::TransientFailure {
                    message: format!("Failed to build HTTP client: {e}"),
                };
            }
        };

        // JSON, like the exchange — this endpoint rejects form encoding.
        let body = serde_json::json!({
            "grant_type": "refresh_token",
            "client_id": CLIENT_ID,
            "refresh_token": current_rt,
        });

        let response = match client
            .post(token_url())
            .header(reqwest::header::ACCEPT, "application/json")
            .json(&body)
            .send()
            .await
        {
            Ok(res) => res,
            Err(e) => {
                return RefreshOutcome::TransientFailure {
                    message: format!("Anthropic refresh network error: {e}"),
                };
            }
        };

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            tracing::warn!(status = %status, body = %body, "Anthropic token refresh failed");
            if status == reqwest::StatusCode::BAD_REQUEST
                || status == reqwest::StatusCode::UNAUTHORIZED
            {
                return RefreshOutcome::PermanentFailure {
                    error: RefreshTokenFailedReason::RefreshTokenRejected.into(),
                    tried_key: Some(current_auth.key),
                    tried_refresh_token: Some(current_rt),
                };
            }
            return RefreshOutcome::TransientFailure {
                message: format!("Anthropic refresh HTTP error ({status}): {body}"),
            };
        }

        let resp: AnthropicRefreshResponse = match response.json().await {
            Ok(r) => r,
            Err(e) => {
                return RefreshOutcome::TransientFailure {
                    message: format!("Failed to parse Anthropic refresh response: {e}"),
                };
            }
        };

        let now = Utc::now();
        let expires_in_secs = resp.expires_in.unwrap_or(3600);
        let expires_at =
            now + chrono::Duration::seconds((expires_in_secs as i64) - EXPIRY_MARGIN_SECS);

        // Keep existing refresh_token if new one not sent in response
        let final_rt = resp.refresh_token.or(Some(current_rt));

        let updated_auth = GrokAuth {
            provider: SubscriptionProvider::Anthropic,
            key: resp.access_token,
            auth_mode: AuthMode::SubscriptionOauth,
            create_time: now,
            user_id: current_auth.user_id,
            refresh_token: final_rt,
            expires_at: Some(expires_at),
            ..current_auth
        };

        RefreshOutcome::success(updated_auth)
    }
}
