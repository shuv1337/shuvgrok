use serde::{Deserialize, Serialize};

/// Access gate from `grok_build_access_gate`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GateInfo {
    pub message: String,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
}

/// Typed auth metadata passed from the shell to the pager via ACP.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthMeta {
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub auth_mode: Option<String>,
    /// Team principal UUID when the session is a team login (`None` for personal).
    #[serde(default)]
    pub team_id: Option<String>,
    #[serde(default)]
    pub team_name: Option<String>,
    #[serde(default)]
    pub is_zdr: bool,
    #[serde(default)]
    pub team_role: Option<String>,
    /// Defaults to opted-out (safer) until auth meta is populated.
    #[serde(default = "crate::auth::default_coding_data_retention_opt_out")]
    pub coding_data_retention_opt_out: bool,
    #[serde(default)]
    pub show_resolved_model: Option<bool>,
    /// `Some` = user is blocked; `None` = user has access.
    #[serde(default)]
    pub gate: Option<GateInfo>,
    /// User-friendly display name for the current subscription tier
    /// (e.g. "SuperGrok Heavy", "X Premium", "Free"). From CCP `/settings`.
    #[serde(default)]
    pub subscription_tier: Option<String>,
    /// Whether `/feedback` may offer a one-shot trace upload; carried on auth
    /// meta so it refreshes with auth changes.
    #[serde(default)]
    pub feedback_trace_offer: bool,
    /// Third-party subscription providers enabled in this build and whether
    /// each is signed in. Drives the `/usage` Subscriptions section. Empty
    /// when the alt-provider feature is off, so clients render nothing.
    #[serde(default)]
    pub subscription_providers: Vec<SubscriptionProviderStatus>,
}

/// One provider row for the `/usage` pane.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SubscriptionProviderStatus {
    /// Canonical id, also the `grok login --provider` argument.
    pub id: String,
    pub display_name: String,
    pub connected: bool,
    /// Plan name when the provider reports one.
    #[serde(default)]
    pub tier: Option<String>,
}

impl SubscriptionProviderStatus {
    /// Status for each enabled third-party provider, read from `auth.json`.
    ///
    /// Reports *stored credentials*, not remaining quota: neither Anthropic
    /// nor OpenAI publishes a subscription-usage endpoint, so there is no
    /// honest number to show for consumption.
    pub fn detect_all() -> Vec<Self> {
        use crate::auth::SubscriptionProvider;
        let grok_home = crate::util::grok_home::grok_home();
        let live = crate::auth::LiveProviders::detect(&grok_home);
        let path = std::env::var("GROK_AUTH_PATH")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| grok_home.join("auth.json"));
        let store = crate::auth::read_auth_json(&path).unwrap_or_default();
        SubscriptionProvider::enabled()
            .into_iter()
            .filter(|p| *p != SubscriptionProvider::Xai)
            .map(|p| Self {
                id: p.id().to_string(),
                display_name: p.display_name().to_string(),
                connected: live.has(p),
                tier: p
                    .static_auth_scope()
                    .and_then(|scope| store.get(scope))
                    .and_then(|a| a.subscription_tier.clone())
                    .filter(|t| !t.trim().is_empty()),
            })
            .collect()
    }
}

impl Default for AuthMeta {
    fn default() -> Self {
        Self {
            email: None,
            auth_mode: None,
            team_id: None,
            team_name: None,
            is_zdr: false,
            team_role: None,
            coding_data_retention_opt_out: crate::auth::default_coding_data_retention_opt_out(),
            show_resolved_model: None,
            gate: None,
            subscription_tier: None,
            feedback_trace_offer: false,
            subscription_providers: Vec::new(),
        }
    }
}
