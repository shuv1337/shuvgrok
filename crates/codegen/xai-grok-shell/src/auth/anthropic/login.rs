use std::io::IsTerminal;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::Utc;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use super::wire::*;
use crate::auth::model::{AuthMode, GrokAuth};
use crate::auth::pkce_loopback::{
    Callback, LoopbackListener, LoopbackPort, race_callback_and_client_ui, race_callback_and_stdin,
};
use crate::auth::providers::SubscriptionProvider;
use crate::auth::{AuthChannels, AuthManager, AuthUrlInfo, AuthUrlMode};

#[derive(Debug)]
pub struct AnthropicPkce {
    pub code_verifier: String,
    pub code_challenge: String,
}

pub fn generate_pkce() -> AnthropicPkce {
    let random_bytes: [u8; 32] = rand::random();
    let code_verifier = URL_SAFE_NO_PAD.encode(random_bytes);
    let code_challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(code_verifier.as_bytes()));
    AnthropicPkce {
        code_verifier,
        code_challenge,
    }
}

pub fn build_authorize_url(redirect_uri: &str, pkce: &AnthropicPkce) -> String {
    let encoded_scopes = urlencoding::encode(SCOPES);
    let encoded_redirect = urlencoding::encode(redirect_uri);
    // For Anthropic OAuth: state = verifier itself
    format!(
        "{AUTHORIZE_URL}?code=true&response_type=code&client_id={CLIENT_ID}&redirect_uri={encoded_redirect}&scope={encoded_scopes}&code_challenge={}&code_challenge_method=S256&state={}",
        pkce.code_challenge, pkce.code_verifier
    )
}

#[derive(Deserialize)]
pub struct AnthropicTokenResponse {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub expires_in: Option<u64>,
}

/// The callback page can render the code as `code#state`; keep only the code.
fn strip_state_suffix(code: &str) -> &str {
    code.split_once('#').map_or(code, |(c, _)| c)
}

/// Exchange the authorization code for tokens.
///
/// Wire shape is **JSON**, matching the reference loopback implementation
/// (`shuvpi/packages/ai/src/auth/oauth/anthropic.ts`). Two details are load
/// bearing and cost a 400 `invalid_request_error` if dropped:
///  * the body must be JSON — this endpoint rejects form encoding on the
///    loopback redirect, and
///  * `state` must be present. For Anthropic the state *is* the PKCE verifier.
///
/// `redirect_uri` must be byte-identical to the authorize call's.
pub async fn exchange_code(
    code: &str,
    verifier: &str,
    state: &str,
    redirect_uri: &str,
) -> anyhow::Result<AnthropicTokenResponse> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?;

    // A bare-code paste carries no state; the verifier is the correct value.
    let state = if state.is_empty() { verifier } else { state };
    let body = serde_json::json!({
        "grant_type": "authorization_code",
        "client_id": CLIENT_ID,
        "code": strip_state_suffix(code),
        "state": state,
        "redirect_uri": redirect_uri,
        "code_verifier": verifier,
    });

    let response = client
        .post(token_url())
        .header(reqwest::header::ACCEPT, "application/json")
        .json(&body)
        .send()
        .await?;

    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        tracing::error!(status = %status, body = %body, "Anthropic token exchange failed");
        anyhow::bail!("Token exchange failed ({status}): {body}");
    }

    let tokens: AnthropicTokenResponse = response.json().await?;
    Ok(tokens)
}

pub async fn run_anthropic_login(
    auth_manager: &Arc<AuthManager>,
    channels: Option<AuthChannels>,
) -> anyhow::Result<(GrokAuth, bool)> {
    tracing::info!("Anthropic: starting OAuth login flow");

    let pkce = generate_pkce();
    let listener = LoopbackListener::bind(LoopbackPort::Fixed {
        port: CALLBACK_PORT,
        path: CALLBACK_PATH,
        redirect_uri: CALLBACK_URL,
    })
    .await?;

    let redirect_uri = listener.redirect_uri();
    let auth_url = build_authorize_url(&redirect_uri, &pkce);

    let (url_tx, code_rx) = match channels {
        Some(ch) => (ch.url_tx, Some(ch.code_rx)),
        None => (None, None),
    };
    let has_client_ui = code_rx.is_some();

    if has_client_ui {
        if let Err(e) = webbrowser::open(&auth_url) {
            tracing::debug!(error = %e, "Anthropic: failed to open browser");
        }
    } else {
        eprintln!();
        eprintln!("Signing in with Claude (Anthropic)...");
        eprintln!();
        if let Err(e) = webbrowser::open(&auth_url) {
            tracing::debug!(error = %e, "Anthropic: failed to open browser");
        }
        eprintln!("Open this URL to sign in:");
        eprintln!("  {}", auth_url);
    }

    let use_stdin = !has_client_ui && std::io::stdin().is_terminal();
    if use_stdin {
        eprintln!();
        eprintln!(
            "{}",
            crate::auth::pkce_loopback::LOOPBACK_PASTE_STDIN_PROMPT
        );
    }

    if let Some(tx) = url_tx {
        let _ = tx.send(AuthUrlInfo {
            url: auth_url.clone(),
            mode: AuthUrlMode::Loopback,
            provider: Some(SubscriptionProvider::Anthropic),
        });
    }

    let Callback {
        code,
        state: received_state,
    } = if let Some(mut rx) = code_rx {
        race_callback_and_client_ui(listener, &mut rx, false).await?
    } else {
        race_callback_and_stdin(listener, use_stdin, false).await?
    };

    if !received_state.is_empty() && received_state != pkce.code_verifier {
        anyhow::bail!("OAuth state mismatch: expected verifier, got {received_state}");
    }

    let tokens = exchange_code(&code, &pkce.code_verifier, &received_state, &redirect_uri).await?;
    let now = Utc::now();
    let expires_in_secs = tokens.expires_in.unwrap_or(3600);
    let expires_at = now + chrono::Duration::seconds((expires_in_secs as i64) - EXPIRY_MARGIN_SECS);

    let auth = GrokAuth {
        provider: SubscriptionProvider::Anthropic,
        key: tokens.access_token,
        auth_mode: AuthMode::SubscriptionOauth,
        create_time: now,
        user_id: String::new(),
        refresh_token: tokens.refresh_token,
        expires_at: Some(expires_at),
        ..Default::default()
    };

    auth_manager
        .update(auth.clone())
        .await
        .map_err(|e| anyhow::anyhow!("Failed to save Anthropic credentials: {e}"))?;

    tracing::info!("Anthropic: OAuth login complete, credentials saved under {AUTH_SCOPE}");
    Ok((auth, true))
}
