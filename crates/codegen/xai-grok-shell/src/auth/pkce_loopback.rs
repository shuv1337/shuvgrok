//! Generic PKCE OAuth loopback listener, paste fallback, and race orchestration.

use std::collections::HashMap;
use std::io::IsTerminal;
use std::sync::Arc;
use std::time::Duration;

use axum::{
    Router,
    extract::{Query, State},
    http::{Method, StatusCode},
    response::Html,
    routing::get,
};
use tokio::net::TcpListener;

pub const AUTH_CALLBACK_TIMEOUT: Duration = Duration::from_secs(600);
pub const LOOPBACK_PORT_OVERRIDE_ENV: &str = "GROK_OAUTH_LOOPBACK_PORT_OVERRIDE";

/// CLI stdin prompt when racing a loopback callback against a paste.
///
/// Remote browsers land on `http://localhost/...` and fail; the address bar
/// still holds `?code=` (and usually `state=`). Pasting that full URL is the
/// supported recovery path.
pub const LOOPBACK_PASTE_STDIN_PROMPT: &str =
    "If localhost fails, paste the full callback URL from the address bar:";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Callback {
    pub code: String,
    pub state: String,
}

pub type CallbackResult = Result<Callback, String>;

#[derive(Clone, Copy, Debug)]
pub enum LoopbackPort {
    /// OS-assigned port; the redirect URI is derived from the bound port.
    /// Used by xAI, whose IdP treats loopback redirects as port-agnostic.
    Ephemeral,
    /// Fixed, pre-registered redirect.
    ///
    /// `redirect_uri` MUST be byte-identical to the string the provider
    /// registered: OAuth servers compare it exactly, and `localhost` and
    /// `127.0.0.1` are *different strings* to them even though they resolve to
    /// the same host. Reconstructing it from the bound socket address is what
    /// produced Anthropic's "Redirect URI ... is not supported by client".
    Fixed {
        port: u16,
        path: &'static str,
        redirect_uri: &'static str,
    },
}

pub enum LoopbackListener {
    Bound {
        listener: TcpListener,
        port: u16,
        path: &'static str,
        /// Pre-registered redirect to send verbatim; `None` derives it from
        /// the bound port (the ephemeral/xAI case).
        registered_redirect: Option<&'static str>,
    },
    PasteOnly {
        port: u16,
        path: &'static str,
        registered_redirect: Option<&'static str>,
        reason: String,
    },
}

impl LoopbackListener {
    pub async fn bind(mode: LoopbackPort) -> anyhow::Result<Self> {
        let env_override = std::env::var(LOOPBACK_PORT_OVERRIDE_ENV)
            .ok()
            .and_then(|v| v.parse::<u16>().ok());

        match mode {
            LoopbackPort::Ephemeral => {
                let port = env_override.unwrap_or(0);
                let listener = TcpListener::bind(("127.0.0.1", port)).await?;
                let bound_port = listener.local_addr()?.port();
                Ok(Self::Bound {
                    listener,
                    port: bound_port,
                    path: "/callback",
                    registered_redirect: None,
                })
            }
            LoopbackPort::Fixed {
                port,
                path,
                redirect_uri,
            } => {
                let target_port = env_override.unwrap_or(port);
                // An override repoints the listener for tests; the registered
                // redirect then no longer describes it, so derive instead.
                let registered_redirect = (env_override.is_none()).then_some(redirect_uri);
                match TcpListener::bind(("127.0.0.1", target_port)).await {
                    Ok(listener) => Ok(Self::Bound {
                        listener,
                        port: target_port,
                        path,
                        registered_redirect,
                    }),
                    Err(e) => {
                        tracing::warn!(
                            port = target_port,
                            error = %e,
                            "OAuth: fixed loopback port could not be bound; falling back to paste-only capture"
                        );
                        Ok(Self::PasteOnly {
                            port: target_port,
                            path,
                            registered_redirect,
                            reason: e.to_string(),
                        })
                    }
                }
            }
        }
    }

    /// The `redirect_uri` to send on the authorize *and* token calls.
    ///
    /// For a pre-registered provider this is the registered constant verbatim,
    /// not a reconstruction: the provider string-matches it, so `localhost`
    /// must not silently become `127.0.0.1`. Both calls read this one method,
    /// so the two can never disagree — the token exchange also requires the
    /// value to match the one used at authorize.
    pub fn redirect_uri(&self) -> String {
        match self {
            Self::Bound {
                port,
                path,
                registered_redirect,
                ..
            }
            | Self::PasteOnly {
                port,
                path,
                registered_redirect,
                ..
            } => registered_redirect
                .map(str::to_owned)
                .unwrap_or_else(|| format!("http://127.0.0.1:{port}{path}")),
        }
    }

    pub fn is_paste_only(&self) -> bool {
        matches!(self, Self::PasteOnly { .. })
    }
}

/// Whether this process likely has a GUI browser.
///
/// Mirrors `xai_grok_pager_render::link_opener` so login code does not depend
/// on the pager crate. Linux/BSD require a non-empty `DISPLAY` or
/// `WAYLAND_DISPLAY` (or a `BROWSER` override). macOS/Windows are treated as
/// available at the env level.
pub fn browser_open_likely_available() -> bool {
    browser_open_likely_available_from_env(
        std::env::var("BROWSER").ok().as_deref(),
        std::env::var("DISPLAY").ok().as_deref(),
        std::env::var("WAYLAND_DISPLAY").ok().as_deref(),
    )
}

/// Pure helper for tests. See [`browser_open_likely_available`].
pub fn browser_open_likely_available_from_env(
    browser: Option<&str>,
    display: Option<&str>,
    wayland_display: Option<&str>,
) -> bool {
    if cfg!(any(target_os = "macos", target_os = "windows")) {
        return true;
    }
    if browser.is_some_and(|v| !v.is_empty()) {
        return true;
    }
    wayland_display.is_some_and(|v| !v.is_empty()) || display.is_some_and(|v| !v.is_empty())
}

/// Parse user-pasted input into `(code, state)`.
///
/// Accepts:
///   1. Full callback URL (`http://127.0.0.1:…` or `http://localhost:…`)
///   2. Form `code#state`
///   3. Bare authorization code: `abc123`
pub fn parse_pasted_input(input: &str) -> Result<Callback, String> {
    let input = input.trim();
    if input.is_empty() {
        return Err("empty input".into());
    }

    // Try parsing as URL first.
    if let Ok(url) = url::Url::parse(input) {
        let params: HashMap<String, String> = url.query_pairs().into_owned().collect();
        if let Some(code) = params.get("code") {
            let state = params.get("state").cloned().unwrap_or_default();
            return Ok(Callback {
                code: code.clone(),
                state,
            });
        }
        if let Some(error) = params.get("error") {
            let desc = params.get("error_description").cloned().unwrap_or_default();
            return Err(if desc.is_empty() {
                error.clone()
            } else {
                format!("{error}: {desc}")
            });
        }
        return Err("URL has no 'code' query parameter".into());
    }

    // Try parsing as `code#state` format.
    if let Some((code, state)) = input.split_once('#') {
        let code = code.trim();
        let state = state.trim();
        if !code.is_empty() {
            return Ok(Callback {
                code: code.to_owned(),
                state: state.to_owned(),
            });
        }
    }

    Ok(Callback {
        code: input.to_owned(),
        state: String::new(),
    })
}

/// Render a styled callback page shown in the browser after the OAuth redirect.
pub fn callback_page(title: &str, message: &str, is_success: bool) -> String {
    let icon = if is_success {
        // Checkmark / brand icon
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="48" height="48" fill="none" viewBox="0 0 24 24" stroke="currentColor" stroke-width="2"><path stroke-linecap="round" stroke-linejoin="round" d="M5 13l4 4L19 7" /></svg>"#
    } else {
        // X circle
        r#"<svg width="48" height="48" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" style="color:#ef4444"><circle cx="12" cy="12" r="10"/><line x1="15" y1="9" x2="9" y2="15"/><line x1="9" y1="9" x2="15" y2="15"/></svg>"#
    };
    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8"/>
<meta name="viewport" content="width=device-width,initial-scale=1"/>
<meta name="color-scheme" content="light dark"/>
<title>{title}</title>
<style>
  *{{margin:0;padding:0;box-sizing:border-box}}
  body{{font-family:-apple-system,BlinkMacSystemFont,"Segoe UI",Roboto,Helvetica,Arial,sans-serif;
    display:flex;align-items:center;justify-content:center;min-height:100vh;
    background:#0a0a0a;color:#e5e5e5}}
  .card{{text-align:center;display:flex;flex-direction:column;align-items:center;gap:16px;padding:48px}}
  h1{{font-size:18px;font-weight:600}}
  p{{font-size:14px;color:#a3a3a3}}
  @media(prefers-color-scheme:light){{
    body{{background:#fafafa;color:#171717}}
    p{{color:#525252}}
  }}
</style>
</head>
<body>
  <div class="card">
    {icon}
    <h1>{title}</h1>
    <p>{message}</p>
  </div>
</body>
</html>"#,
        title = title,
        icon = icon,
        message = message,
    )
}

fn build_callback_router(
    path: &'static str,
    tx: tokio::sync::mpsc::Sender<CallbackResult>,
    enable_accounts_cors: bool,
) -> Router {
    let mut router = Router::new().route(path, get(handle_callback));
    if enable_accounts_cors {
        let cors =
            crate::auth::config::accounts_app_cors_layer(Method::GET).allow_private_network(true);
        router = router.layer(cors);
    }
    router.with_state(tx)
}

async fn handle_callback(
    State(tx): State<tokio::sync::mpsc::Sender<CallbackResult>>,
    Query(params): Query<HashMap<String, String>>,
) -> (StatusCode, Html<String>) {
    let result = parse_callback_params(&params);
    let response = callback_response(&result);
    if let Err(e) = tx.try_send(result) {
        tracing::error!(
            ?e,
            "OAuth: callback channel send failed; auth will time out"
        );
    }
    response
}

fn parse_callback_params(params: &HashMap<String, String>) -> CallbackResult {
    if let Some(code) = params.get("code") {
        let state = params.get("state").cloned().unwrap_or_default();
        tracing::debug!(state = %state, "OAuth: received code via loopback callback");
        return Ok(Callback {
            code: code.clone(),
            state,
        });
    }
    let error = params.get("error").cloned().unwrap_or_default();
    let desc = params.get("error_description").cloned().unwrap_or_default();
    tracing::error!(error = %error, desc = %desc, "OAuth: IdP returned error");
    Err(if desc.is_empty() {
        error
    } else {
        format!("{error}: {desc}")
    })
}

fn callback_response(result: &CallbackResult) -> (StatusCode, Html<String>) {
    let (title, message) = match result {
        Ok(_) => (
            "Signed in",
            "You can close this window and return to the application.",
        ),
        Err(_) => ("Access denied", "Close this window and try again."),
    };
    (
        StatusCode::OK,
        Html(callback_page(title, message, result.is_ok())),
    )
}

#[cfg(unix)]
fn wait_for_stdin_or_closed(
    stdin: &std::io::Stdin,
    tx: &tokio::sync::mpsc::Sender<CallbackResult>,
) -> bool {
    use std::os::unix::io::AsRawFd;
    let fd = stdin.as_raw_fd();
    loop {
        if tx.is_closed() {
            return false;
        }
        let ready = unsafe {
            let mut fds = std::mem::zeroed::<libc::pollfd>();
            fds.fd = fd;
            fds.events = libc::POLLIN;
            libc::poll(&mut fds, 1, 200)
        };
        if ready > 0 {
            return true;
        }
    }
}

fn spawn_stdin_reader(tx: tokio::sync::mpsc::Sender<CallbackResult>) {
    tokio::task::spawn_blocking(move || {
        use std::io::BufRead;
        let stdin = std::io::stdin();
        let mut buf = String::new();
        loop {
            #[cfg(unix)]
            if !wait_for_stdin_or_closed(&stdin, &tx) {
                tracing::debug!("OAuth: stdin reader exiting, channel closed");
                return;
            }
            #[cfg(not(unix))]
            if tx.is_closed() {
                tracing::debug!("OAuth: stdin reader exiting, channel closed");
                return;
            }

            buf.clear();
            let mut handle = stdin.lock();
            match handle.read_line(&mut buf) {
                Ok(0) => return,
                Ok(_) => {}
                Err(_) => return,
            }
            drop(handle);

            let trimmed = buf.trim().to_owned();
            if trimmed.is_empty() {
                continue;
            }
            match parse_pasted_input(&trimmed) {
                Ok(result) => {
                    tracing::debug!("OAuth: received code via stdin paste");
                    let _ = tx.blocking_send(Ok(result));
                    return;
                }
                Err(msg) => {
                    tracing::debug!(input = %msg, "OAuth: invalid stdin paste, retrying");
                    eprintln!("  Invalid input: {msg}. Try again:");
                }
            }
        }
    });
}

/// Race loopback callback against manual paste from `code_rx`.
pub async fn race_callback_and_client_ui(
    listener: LoopbackListener,
    code_rx: &mut tokio::sync::mpsc::Receiver<String>,
    enable_accounts_cors: bool,
) -> anyhow::Result<Callback> {
    tracing::debug!("OAuth: waiting for auth code (loopback + client paste)");
    let (tx, mut rx) = tokio::sync::mpsc::channel::<CallbackResult>(1);
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();

    let server = match listener {
        LoopbackListener::Bound { listener, path, .. } => {
            let app = build_callback_router(path, tx.clone(), enable_accounts_cors);
            Some(tokio::spawn(async move {
                let _ = axum::serve(listener, app)
                    .with_graceful_shutdown(async {
                        let _ = shutdown_rx.await;
                    })
                    .await;
            }))
        }
        LoopbackListener::PasteOnly { .. } => None,
    };

    let client_tx = tx.clone();
    let client_bridge = async {
        while let Some(code) = code_rx.recv().await {
            match parse_pasted_input(&code) {
                Ok(result) => {
                    tracing::debug!("OAuth: received code via client paste");
                    let _ = client_tx.send(Ok(result)).await;
                    return;
                }
                Err(e) => {
                    tracing::debug!(error = %e, "OAuth: invalid client paste input");
                }
            }
        }
    };

    drop(tx);

    let result = tokio::select! {
        r = tokio::time::timeout(AUTH_CALLBACK_TIMEOUT, rx.recv()) => {
            r.map_err(|_| anyhow::anyhow!("Timed out waiting for authentication"))?
                .ok_or_else(|| anyhow::anyhow!("Authentication callback channel closed"))?
        }
        _ = client_bridge => {
            rx.recv().await
                .ok_or_else(|| anyhow::anyhow!("Authentication callback channel closed"))?
        }
    };

    let _ = shutdown_tx.send(());
    if let Some(s) = server {
        let _ = s.await;
    }

    result.map_err(|e| anyhow::anyhow!("Authentication failed: {e}"))
}

/// Race loopback callback against stdin paste.
pub async fn race_callback_and_stdin(
    listener: LoopbackListener,
    enable_stdin: bool,
    enable_accounts_cors: bool,
) -> anyhow::Result<Callback> {
    tracing::debug!(
        enable_stdin = enable_stdin,
        "OAuth: waiting for auth code (loopback + stdin)"
    );
    let (tx, mut rx) = tokio::sync::mpsc::channel::<CallbackResult>(1);
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();

    let server = match listener {
        LoopbackListener::Bound { listener, path, .. } => {
            let app = build_callback_router(path, tx.clone(), enable_accounts_cors);
            Some(tokio::spawn(async move {
                let _ = axum::serve(listener, app)
                    .with_graceful_shutdown(async {
                        let _ = shutdown_rx.await;
                    })
                    .await;
            }))
        }
        LoopbackListener::PasteOnly { .. } => None,
    };

    if enable_stdin {
        spawn_stdin_reader(tx.clone());
    }

    drop(tx);

    let result = tokio::time::timeout(AUTH_CALLBACK_TIMEOUT, rx.recv())
        .await
        .map_err(|_| {
            tracing::error!("auth: timed out after 10 minutes waiting for auth code");
            anyhow::anyhow!("Timed out waiting for authentication code")
        })?
        .ok_or_else(|| {
            tracing::error!(
                "OAuth: callback channel closed, no code received from loopback or stdin"
            );
            anyhow::anyhow!("Authentication callback channel closed")
        })?;

    let _ = shutdown_tx.send(());
    if let Some(s) = server {
        let _ = s.await;
    }

    result.map_err(|e| anyhow::anyhow!("Authentication failed: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// FROZEN redirect contract. Providers string-match `redirect_uri`, so a
    /// pre-registered value must reach the wire byte-identical. Deriving it
    /// from the bound socket turned `localhost` into `127.0.0.1` and Anthropic
    /// rejected the authorize call with "Redirect URI ... is not supported by
    /// client". Ephemeral (xAI) still derives, which is what its IdP expects.
    #[tokio::test]
    async fn fixed_loopback_sends_registered_redirect_verbatim() {
        let listener = LoopbackListener::bind(LoopbackPort::Fixed {
            port: 0, // OS-assigned; the redirect must still be the constant
            path: "/callback",
            redirect_uri: "http://localhost:53692/callback",
        })
        .await
        .expect("bind");
        assert_eq!(listener.redirect_uri(), "http://localhost:53692/callback");

        let ephemeral = LoopbackListener::bind(LoopbackPort::Ephemeral)
            .await
            .expect("bind");
        let derived = ephemeral.redirect_uri();
        assert!(
            derived.starts_with("http://127.0.0.1:") && derived.ends_with("/callback"),
            "ephemeral must derive from the bound port, got {derived}"
        );
    }

    /// The provider constants are what each vendor registered; a typo here is
    /// an authorize-time failure that no unit test would otherwise catch.
    #[test]
    fn provider_callback_constants_match_registered_values() {
        use crate::auth::anthropic::wire as ant;
        use crate::auth::openai_codex::wire as codex;

        assert_eq!(ant::CALLBACK_URL, "http://localhost:53692/callback");
        assert_eq!(ant::CALLBACK_PORT, 53692);
        assert_eq!(ant::CALLBACK_PATH, "/callback");

        assert_eq!(codex::CALLBACK_URL, "http://localhost:1455/auth/callback");
        assert_eq!(codex::CALLBACK_PORT, 1455);
        assert_eq!(codex::CALLBACK_PATH, "/auth/callback");

        // The constant must agree with its own port/path parts.
        assert_eq!(
            ant::CALLBACK_URL,
            format!(
                "http://localhost:{}{}",
                ant::CALLBACK_PORT,
                ant::CALLBACK_PATH
            )
        );
        assert_eq!(
            codex::CALLBACK_URL,
            format!(
                "http://localhost:{}{}",
                codex::CALLBACK_PORT,
                codex::CALLBACK_PATH
            )
        );
    }

    #[test]
    fn parse_pasted_input_full_url() {
        let input = "http://127.0.0.1:53692/callback?code=abc123code&state=verifier123";
        let res = parse_pasted_input(input).unwrap();
        assert_eq!(res.code, "abc123code");
        assert_eq!(res.state, "verifier123");
    }

    #[test]
    fn parse_pasted_input_hash_format() {
        let input = "abc123code#verifier123";
        let res = parse_pasted_input(input).unwrap();
        assert_eq!(res.code, "abc123code");
        assert_eq!(res.state, "verifier123");
    }

    #[test]
    fn parse_pasted_input_bare_code() {
        let input = "bare-code-12345";
        let res = parse_pasted_input(input).unwrap();
        assert_eq!(res.code, "bare-code-12345");
        assert_eq!(res.state, "");
    }

    #[test]
    fn parse_pasted_input_openai_localhost_callback() {
        let input = "http://localhost:1455/auth/callback?code=abc123code&state=state-uuid";
        let res = parse_pasted_input(input).unwrap();
        assert_eq!(res.code, "abc123code");
        assert_eq!(res.state, "state-uuid");
    }

    #[test]
    fn linux_without_display_is_headless() {
        if cfg!(any(target_os = "macos", target_os = "windows")) {
            assert!(browser_open_likely_available_from_env(None, None, None));
            return;
        }
        assert!(!browser_open_likely_available_from_env(None, None, None));
        assert!(!browser_open_likely_available_from_env(
            Some(""),
            Some(""),
            Some("")
        ));
        assert!(browser_open_likely_available_from_env(
            None,
            Some(":0"),
            None
        ));
        assert!(browser_open_likely_available_from_env(
            None,
            None,
            Some("wayland-0")
        ));
        assert!(browser_open_likely_available_from_env(
            Some("firefox"),
            None,
            None
        ));
    }
}
