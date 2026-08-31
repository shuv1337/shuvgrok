//! Interactive login orchestration: callback HTTP server, browser
//! handoff, stdin paste fallback, race between the two.
//!
//! Cross-references [`super::protocol`] for OIDC mechanics and
//! [`super::super::AuthManager`] for credential persistence.

use std::io::IsTerminal;
use std::sync::Arc;

pub(crate) use crate::auth::pkce_loopback::{
    Callback, CallbackResult, callback_page, parse_pasted_input as pkce_parse_pasted_input,
};

use super::super::config::{GrokComConfig, OidcAuthConfig};
use super::super::{AuthManager, GrokAuth};
use super::protocol::{
    OidcError, build_authorize_url, build_grok_auth, discover, enforce_login_principal,
    exchange_code, extract_user_info, generate_pkce, login_principal_policy,
    peek_access_token_principal, peek_access_token_principal_id, validate_state,
};

/// Maximum time to wait for the browser OAuth callback (or manual paste of the code).
/// 10 minutes is long enough for users who step away briefly during login.
pub(super) const AUTH_CALLBACK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);

fn parse_pasted_input(input: &str) -> Result<Callback, OidcError> {
    pkce_parse_pasted_input(input).map_err(|e| {
        if e == "empty input" {
            OidcError::InvalidPastedInput("empty input".into())
        } else if e.contains("URL has no 'code'") {
            OidcError::InvalidPastedInput("URL has no 'code' query parameter".into())
        } else {
            OidcError::CallbackAuthFailed(e)
        }
    })
}

/// Run the full OIDC login flow: discovery → PKCE → browser → callback → token exchange → persist.
pub async fn run_login_flow(
    config: &GrokComConfig,
    auth_manager: &Arc<AuthManager>,
    channels: Option<super::super::flow::AuthChannels>,
) -> anyhow::Result<(GrokAuth, bool)> {
    let oidc = config
        .oidc
        .as_ref()
        .ok_or_else(|| anyhow::Error::new(OidcError::NotConfigured))?;
    run_login_flow_with_config(oidc, auth_manager, channels).await
}

/// Run the OIDC login flow with an explicit [`OidcAuthConfig`].
///
/// Also used by the OAuth2 provider path via [`OAuth2ProviderConfig::as_oidc`].
///
/// The flow races two input paths:
///   - **Path A**: A loopback HTTP server on `127.0.0.1` that receives the IdP redirect.
///   - **Path B**: Stdin paste — the user manually pastes the callback URL or bare auth code.
///
/// Path B is essential for remote VMs where the browser runs on a different machine
/// and the `127.0.0.1` redirect cannot reach the CLI process.
/// * `channels` — `Some`: pushes the auth URL to the TUI and receives pasted codes.
///   `None`: prints to stderr / reads stdin (CLI mode).
pub async fn run_login_flow_with_config(
    oidc: &OidcAuthConfig,
    auth_manager: &Arc<AuthManager>,
    channels: Option<super::super::flow::AuthChannels>,
) -> anyhow::Result<(GrokAuth, bool)> {
    tracing::info!(issuer = %oidc.issuer, client_id = %oidc.client_id, "OIDC: starting login flow");

    // Ensure jsonwebtoken CryptoProvider is installed (required for JWT validation).
    jsonwebtoken::crypto::CryptoProvider::install_default(
        &jsonwebtoken::crypto::rust_crypto::DEFAULT_PROVIDER,
    )
    .ok();

    let discovery = discover(&oidc.issuer).await?;
    let pkce = generate_pkce();
    let state = uuid::Uuid::now_v7().to_string();
    let nonce = uuid::Uuid::now_v7().to_string();

    // In local-dev mode, use a fixed callback port so the redirect_uri is stable
    // and can be pre-registered with the local OAuth2 provider. In production the
    // OS picks a random available port.
    let loopback_mode = if super::super::config::use_local_auth() {
        // Local dev pre-registers the 127.0.0.1 form, which is what this flow
        // has always sent — keep deriving it rather than pinning a constant.
        crate::auth::pkce_loopback::LoopbackPort::Fixed {
            port: 56121,
            path: "/callback",
            redirect_uri: "http://127.0.0.1:56121/callback",
        }
    } else {
        crate::auth::pkce_loopback::LoopbackPort::Ephemeral
    };
    let listener = crate::auth::pkce_loopback::LoopbackListener::bind(loopback_mode)
        .await
        .map_err(|e| anyhow::Error::new(OidcError::BindLoopback(e.to_string())))?;
    let redirect_uri = listener.redirect_uri();
    let oauth2 = auth_manager.grok_com_config().oauth2.as_ref();
    let auth_url = build_authorize_url(
        oidc,
        oauth2,
        &discovery,
        &redirect_uri,
        &pkce,
        &state,
        &nonce,
    );
    tracing::debug!(redirect_uri = %redirect_uri, "OIDC: callback server bound");

    let (url_tx, code_rx) = match channels {
        Some(ch) => (ch.url_tx, Some(ch.code_rx)),
        None => (None, None),
    };
    let has_client_ui = code_rx.is_some();

    if has_client_ui {
        // Client provides its own auth UI; just open the browser.
        if let Err(e) = webbrowser::open(&auth_url) {
            tracing::debug!(error = %e, "OIDC: failed to open browser");
        }
    } else {
        // No client UI — print to stderr.
        eprintln!();
        let provider_label = if oidc.issuer == super::super::config::XAI_OAUTH2_ISSUER {
            "Grok".to_owned()
        } else {
            oidc.issuer.clone()
        };
        eprintln!("Signing in with {}...", provider_label);
        eprintln!();
        if let Err(e) = webbrowser::open(&auth_url) {
            tracing::debug!(error = %e, "OIDC: failed to open browser");
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

    // Push auth URL to the TUI via oneshot.
    if let Some(tx) = url_tx {
        let _ = tx.send(super::super::flow::AuthUrlInfo {
            url: auth_url.clone(),
            mode: super::super::flow::AuthUrlMode::Loopback,
            provider: Some(crate::auth::SubscriptionProvider::Xai),
        });
    }

    let crate::auth::pkce_loopback::Callback {
        code,
        state: received_state,
    } = if let Some(mut rx) = code_rx {
        // Client UI: race loopback against manual paste via code_rx.
        crate::auth::pkce_loopback::race_callback_and_client_ui(listener, &mut rx, true).await?
    } else {
        // No client UI: race loopback against stdin paste.
        crate::auth::pkce_loopback::race_callback_and_stdin(listener, use_stdin, true).await?
    };

    // Validate state (skip for bare code paste where state is empty)
    if !received_state.is_empty() {
        validate_state(&state, &received_state)?;
    }

    let tokens = exchange_code(
        &discovery.token_endpoint,
        &code,
        &redirect_uri,
        &oidc.client_id,
        &pkce.code_verifier,
    )
    .await?;
    tracing::info!(
        has_refresh = tokens.refresh_token.is_some(),
        expires_in = ?tokens.expires_in,
        "OIDC: token exchange complete"
    );

    // Resolve the actual principal chosen on the consent screen.
    //
    // The shell's config may not have principal_type set (personal login),
    // but the user might pick "Team" on the consent screen. The server
    // encodes the chosen principal in the access token JWT. If the config
    // doesn't specify a principal, peek at the token to discover it.
    let token_principal = peek_access_token_principal(&tokens.access_token);

    // The authorize URL only pre-selects; verify the token's principal here.
    // Match the principal id even if `principal_type` is absent.
    let principal_policy = login_principal_policy(auth_manager.grok_com_config());
    enforce_login_principal(
        principal_policy.as_ref(),
        peek_access_token_principal_id(&tokens.access_token).as_deref(),
    )?;

    let (resolved_principal_type, resolved_principal_id, resolved_team_id) = {
        let cfg_pt = oauth2.and_then(|cfg| cfg.principal_type.clone());
        let cfg_pid = oauth2.and_then(|cfg| cfg.principal_id.clone());
        if cfg_pt.is_some() {
            (cfg_pt, cfg_pid, None)
        } else if let Some((pt, pid, tid)) = token_principal {
            tracing::info!(
                principal_type = %pt,
                principal_id = %pid,
                team_id = ?tid,
                "OIDC: resolved principal from access token"
            );
            (Some(pt), Some(pid), tid)
        } else {
            (cfg_pt, cfg_pid, None)
        }
    };

    let user_info = extract_user_info(
        tokens.id_token.as_deref(),
        &discovery,
        &oidc.issuer,
        &oidc.client_id,
        &nonce,
        resolved_principal_type.as_deref(),
        resolved_principal_id.as_deref(),
        resolved_team_id,
    )
    .await?;
    tracing::debug!(user_id = %user_info.user_id, "OIDC: extracted user info");

    let mut auth = build_grok_auth(tokens, user_info, &oidc.issuer, &oidc.client_id);
    auth_manager.enrich_auth_inline(&mut auth).await;
    let auth = auth_manager
        .update(auth)
        .await
        .map_err(|e| anyhow::Error::new(OidcError::SaveAuth(e.to_string())))?;
    tracing::info!(user_id = %auth.user_id, "OIDC: login complete, credentials saved");

    Ok((auth, true))
}

#[cfg(test)]
mod tests {
    use super::super::test_helpers::*;
    use super::*;

    /// End-to-end test: mock IdP + full login flow with code arriving via loopback.
    /// Exercises discovery → PKCE → race_callback_and_stdin → token exchange → user info → persist.
    #[tokio::test]
    async fn full_login_flow_via_race() {
        ensure_crypto_provider();
        let (issuer, idp_server) = start_mock_idp().await;
        let temp_dir = tempfile::tempdir().unwrap();
        // Dead proxy port: inline `/user` enrichment fails fast in tests.
        let dead_proxy = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            format!("http://127.0.0.1:{}", l.local_addr().unwrap().port())
        };
        let auth_manager = Arc::new(
            AuthManager::new(temp_dir.path(), GrokComConfig::default())
                .with_proxy_base_url(&dead_proxy),
        );

        let oidc_cfg = OidcAuthConfig {
            issuer: issuer.clone(),
            client_id: TEST_CLIENT_ID.into(),
            scopes: vec!["openid".into(), "email".into()],
            audience: None,
        };
        let discovery = discover(&oidc_cfg.issuer).await.unwrap();
        let pkce = generate_pkce();
        let state = "test-state".to_string();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let redirect_uri = format!("http://127.0.0.1:{port}/callback");
        let loopback = crate::auth::pkce_loopback::LoopbackListener::Bound {
            listener,
            port,
            path: "/callback",
            registered_redirect: None,
        };
        let _auth_url = build_authorize_url(
            &oidc_cfg,
            None,
            &discovery,
            &redirect_uri,
            &pkce,
            &state,
            &test_nonce(),
        );

        // Simulate browser callback via race_callback_and_stdin
        let Callback {
            code,
            state: received_state,
        } = tokio::join!(
            crate::auth::pkce_loopback::race_callback_and_stdin(loopback, false, true),
            async {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                reqwest::get(format!(
                    "http://127.0.0.1:{port}/callback?code=mock-auth-code&state={state}"
                ))
                .await
                .unwrap();
            }
        )
        .0
        .unwrap();

        assert_eq!(code, "mock-auth-code");
        assert_eq!(received_state, state);

        let tokens = exchange_code(
            &discovery.token_endpoint,
            &code,
            &redirect_uri,
            &oidc_cfg.client_id,
            &pkce.code_verifier,
        )
        .await
        .unwrap();
        assert_eq!(tokens.access_token, "mock-access-token");

        let user_info = extract_user_info(
            tokens.id_token.as_deref(),
            &discovery,
            &oidc_cfg.issuer,
            &oidc_cfg.client_id,
            &test_nonce(),
            None,
            None,
            None,
        )
        .await
        .unwrap();
        let auth = build_grok_auth(tokens, user_info, &oidc_cfg.issuer, &oidc_cfg.client_id);
        let auth = auth_manager.update(auth).await.unwrap();

        assert_eq!(auth.key, "mock-access-token");
        assert_eq!(auth.refresh_token.as_deref(), Some("mock-refresh-token"));
        assert_eq!(auth.user_id, "user-42");
        assert_eq!(auth.email.as_deref(), Some("test@corp.com"));
        assert!(auth.principal_type.is_none());
        assert!(auth.principal_id.is_none());
        assert!(auth.expires_at.is_some());
        assert_eq!(auth.oidc_issuer.as_deref(), Some(issuer.as_str()));

        let auth_json = std::fs::read_to_string(temp_dir.path().join("auth.json")).unwrap();
        assert!(auth_json.contains("mock-access-token"));
        assert!(auth_json.contains("user-42"));

        idp_server.abort();
    }
    /// Parser matrix: full callback URL, bare code, error URL, empty.
    /// Each case is one bug class:
    ///   - full URL: regression in URL extraction
    ///   - bare code: paste-friendly fallback
    ///   - error URL: surfaces IdP error to user
    ///   - empty: input validation
    #[test]
    fn parse_pasted_input_matrix() {
        // (input, expected: Ok((code, state)) | Err substring)
        let ok_cases: &[(&str, &str, &str)] = &[
            (
                "http://127.0.0.1:54321/callback?code=abc123&state=xyz789",
                "abc123",
                "xyz789",
            ),
            ("abc123def456", "abc123def456", ""),
        ];
        for (input, code, state) in ok_cases {
            let cb =
                parse_pasted_input(input).unwrap_or_else(|e| panic!("parse {input:?} failed: {e}"));
            assert_eq!(cb.code, *code, "code for {input:?}");
            assert_eq!(cb.state, *state, "state for {input:?}");
        }

        let err_cases: &[(&str, &str)] = &[
            (
                "http://127.0.0.1:54321/callback?error=access_denied&error_description=User+denied",
                "access_denied",
            ),
            ("", ""),
            ("   ", ""),
        ];
        for (input, expected_substr) in err_cases {
            let err = parse_pasted_input(input).unwrap_err();
            if !expected_substr.is_empty() {
                assert!(
                    err.to_string().contains(expected_substr),
                    "input {input:?} -> unexpected err: {err}"
                );
            }
        }
    }
}
