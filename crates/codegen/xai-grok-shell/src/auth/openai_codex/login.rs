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
pub struct OpenAiPkce {
    pub code_verifier: String,
    pub code_challenge: String,
    pub state: String,
}

pub fn generate_pkce() -> OpenAiPkce {
    let random_bytes: [u8; 32] = rand::random();
    let code_verifier = URL_SAFE_NO_PAD.encode(random_bytes);
    let code_challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(code_verifier.as_bytes()));
    let state = uuid::Uuid::now_v7().to_string();
    OpenAiPkce {
        code_verifier,
        code_challenge,
        state,
    }
}

pub fn build_authorize_url(redirect_uri: &str, pkce: &OpenAiPkce) -> String {
    let encoded_scopes = urlencoding::encode(SCOPES);
    let encoded_redirect = urlencoding::encode(redirect_uri);
    format!(
        "{AUTHORIZE_URL}?response_type=code&client_id={CLIENT_ID}&redirect_uri={encoded_redirect}&scope={encoded_scopes}&code_challenge={}&code_challenge_method=S256&state={}&id_token_add_organizations=true&codex_cli_simplified_flow=true&originator={ORIGINATOR}",
        pkce.code_challenge, pkce.state
    )
}

#[derive(Deserialize)]
pub(crate) struct OpenAiTokenResponse {
    pub(crate) access_token: String,
    #[serde(default)]
    pub(crate) refresh_token: Option<String>,
    #[serde(default)]
    pub(crate) expires_in: Option<u64>,
}

pub(crate) async fn exchange_code(
    code: &str,
    verifier: &str,
    redirect_uri: &str,
) -> anyhow::Result<OpenAiTokenResponse> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?;

    let params = [
        ("grant_type", "authorization_code"),
        ("client_id", CLIENT_ID),
        ("code", code),
        ("code_verifier", verifier),
        ("redirect_uri", redirect_uri),
    ];

    let response = client.post(token_url()).form(&params).send().await?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        tracing::error!(status = %status, body = %body, "OpenAI token exchange failed");
        anyhow::bail!("OpenAI token exchange failed ({status}): {body}");
    }

    let tokens: OpenAiTokenResponse = response.json().await?;
    Ok(tokens)
}

pub async fn run_openai_codex_login(
    auth_manager: &Arc<AuthManager>,
    channels: Option<AuthChannels>,
) -> anyhow::Result<(GrokAuth, bool)> {
    tracing::info!("OpenAI Codex: starting browser OAuth login flow");

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
            tracing::debug!(error = %e, "OpenAI: failed to open browser");
        }
    } else {
        eprintln!();
        eprintln!("Signing in with ChatGPT (OpenAI)...");
        eprintln!();
        if let Err(e) = webbrowser::open(&auth_url) {
            tracing::debug!(error = %e, "OpenAI: failed to open browser");
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
            provider: Some(SubscriptionProvider::OpenaiCodex),
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

    if !received_state.is_empty() && received_state != pkce.state {
        anyhow::bail!("OAuth state mismatch");
    }

    let tokens = exchange_code(&code, &pkce.code_verifier, &redirect_uri).await?;

    let account_id = extract_chatgpt_account_id(&tokens.access_token).ok_or_else(|| {
        anyhow::anyhow!(
            "Missing chatgpt_account_id in OpenAI access token. Ensure your account has ChatGPT subscription access."
        )
    })?;

    let now = Utc::now();
    let expires_in_secs = tokens.expires_in.unwrap_or(3600);
    let expires_at = now + chrono::Duration::seconds((expires_in_secs as i64) - EXPIRY_MARGIN_SECS);

    let auth = GrokAuth {
        provider: SubscriptionProvider::OpenaiCodex,
        key: tokens.access_token,
        auth_mode: AuthMode::SubscriptionOauth,
        create_time: now,
        user_id: account_id.clone(),
        account_id: Some(account_id),
        refresh_token: tokens.refresh_token,
        expires_at: Some(expires_at),
        ..Default::default()
    };

    auth_manager
        .update(auth.clone())
        .await
        .map_err(|e| anyhow::anyhow!("Failed to save OpenAI Codex credentials: {e}"))?;

    tracing::info!("OpenAI Codex: OAuth login complete, credentials saved under {AUTH_SCOPE}");
    Ok((auth, true))
}
