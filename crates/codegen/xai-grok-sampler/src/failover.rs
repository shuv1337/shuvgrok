//! Endpoint failover pool for the same logical model.
//!
//! [`FailoverState`] tracks per-endpoint health for the ordered pool in
//! [`SamplerConfig::failover_pool`]. Pool index 0 is the first alternate;
//! the primary provider itself (the config's own `base_url`) is
//! conceptually "before" index 0 and never re-entered while a turn runs.
//!
//! Cooldown policy (per endpoint, doubling on repeat failure):
//! - 429 → 60s: free-tier windows reset slowly, a quick revisit is wasted.
//! - other failures (5xx / transport / stream) → 30s.
//! - each consecutive failure doubles the previous cooldown, capped at 10 min.
//! - any success resets the endpoint's counters.

use std::time::{Duration, Instant};

use crate::config::{AuthScheme, FailoverEndpoint, FailoverPool};
use crate::shared_http;

/// First cooldown after one failure on an endpoint.
const RATE_LIMIT_COOLDOWN: Duration = Duration::from_secs(60);
const FAILURE_COOLDOWN: Duration = Duration::from_secs(30);
/// Ceiling for the doubling sequence.
const MAX_COOLDOWN: Duration = Duration::from_secs(600);

/// Liveness-probe timeout. A failover should not stall the turn longer
/// than a normal connect attempt would; unreachable endpoints are skipped
/// almost immediately.
pub(crate) const PROBE_TIMEOUT: Duration = Duration::from_millis(1500);

/// Outcome of probing one candidate before committing to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeOutcome {
    /// 200: reachable and authorized — commit immediately.
    Ok,
    /// 401/403: reachable but this key cannot list models. Some gateways
    /// require a paid key for `/models` yet still serve chat, so this
    /// candidate stays eligible as a last resort.
    ReachableButUnauthorized,
    /// Transport error or any other non-success status.
    Unreachable,
}

impl ProbeOutcome {
    pub fn is_pass(self) -> bool {
        matches!(
            self,
            ProbeOutcome::Ok | ProbeOutcome::ReachableButUnauthorized
        )
    }
}

/// Whether `GROK_FAILOVER_PROBE` enables pre-commit liveness probing.
///
/// Default is ON ("provably available" failover); set the env var to `0`
/// to skip probes entirely. Any value other than `0`/`false` keeps it on,
/// matching the kill-switch conventions elsewhere in this crate.
fn probing_enabled() -> bool {
    match std::env::var("GROK_FAILOVER_PROBE") {
        Ok(v) => !(v == "0" || v.eq_ignore_ascii_case("false")),
        Err(_) => true,
    }
}

/// Per-endpoint health record.
#[derive(Debug, Clone, PartialEq, Eq)]
struct EndpointHealth {
    consecutive_failures: u32,
    last_error_kind: Option<EndpointErrorKind>,
    cooldown_until: Option<Instant>,
    successes: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EndpointErrorKind {
    RateLimited,
    ServerOrTransport,
}

impl Default for EndpointHealth {
    #[allow(clippy::derivable_impls)]
    fn default() -> Self {
        Self {
            consecutive_failures: 0,
            last_error_kind: None,
            cooldown_until: None,
            successes: 0,
        }
    }
}

impl EndpointHealth {
    fn in_cooldown(&self, now: Instant) -> bool {
        self.cooldown_until.is_some_and(|until| until > now)
    }

    fn record_failure(&mut self, kind: EndpointErrorKind, now: Instant) {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        self.last_error_kind = Some(kind);
        let base = match kind {
            EndpointErrorKind::RateLimited => RATE_LIMIT_COOLDOWN,
            EndpointErrorKind::ServerOrTransport => FAILURE_COOLDOWN,
        };
        // Doubling applies within a class: base * 2^(failures-1), capped.
        let shift = self.consecutive_failures.saturating_sub(1);
        let cooldown = base
            .checked_mul(2u32.saturating_pow(shift.min(30)))
            .unwrap_or(MAX_COOLDOWN)
            .min(MAX_COOLDOWN);
        let until = now.checked_add(cooldown).unwrap_or(
            // Saturated clock: treat as cooled-down "forever".
            Instant::now() + MAX_COOLDOWN,
        );
        self.cooldown_until = Some(until);
    }

    fn record_success(&mut self) {
        self.consecutive_failures = 0;
        self.last_error_kind = None;
        self.cooldown_until = None;
        self.successes = self.successes.saturating_add(1);
    }
}

/// Health bookkeeping for one request's failover pool.
///
/// Cheap to construct per task; lives only as long as the request loop.
#[derive(Debug)]
pub struct FailoverState {
    pool_len: usize,
    active: Option<usize>,
    health: Vec<EndpointHealth>,
}

impl FailoverState {
    /// Build state over `pool`. The primary config is not part of the
    /// tracked indices; [`Self::active`] starts at `None`.
    pub fn new(pool: &FailoverPool) -> Self {
        Self {
            pool_len: pool.endpoints.len(),
            active: None,
            health: vec![EndpointHealth::default(); pool.endpoints.len()],
        }
    }

    /// Index of the endpoint currently being sampled (`None` = primary).
    pub fn active(&self) -> Option<usize> {
        self.active
    }

    /// Record a failure against the currently active endpoint (or the
    /// primary when nothing has been committed yet — the primary has no
    /// tracked health, so only the event is logged).
    pub fn mark_active_failed(&mut self, rate_limited: bool) {
        let kind = if rate_limited {
            EndpointErrorKind::RateLimited
        } else {
            EndpointErrorKind::ServerOrTransport
        };
        match self.active {
            Some(idx) => {
                self.health[idx].record_failure(kind, Instant::now());
                tracing::info!(
                    target: crate::sampling_log::TARGET,
                    pool_index = idx + 1,
                    consecutive_failures = self.health[idx].consecutive_failures,
                    rate_limited,
                    "failover endpoint marked failed"
                );
            }
            None => {
                tracing::info!(
                    target: crate::sampling_log::TARGET,
                    rate_limited,
                    "primary endpoint failed; considering pool"
                );
            }
        }
    }

    /// Whether the active endpoint has hit [`FAILOVER_THRESHOLD`]
    /// consecutive failures and the request should hop. The primary
    /// (untracked, index `None`) is always over threshold: it has no
    /// per-request history to accumulate a streak on.
    pub fn threshold_reached_on_active(&self) -> bool {
        match self.active {
            Some(idx) => threshold_reached(self.health[idx].consecutive_failures),
            None => true,
        }
    }

    /// Next healthy pool index rotating after `self.active`, skipping
    /// endpoints that are cooling down. Returns `None` when every
    /// candidate is exhausted (in cooldown or tried).
    pub fn next_healthy(&self) -> Option<usize> {
        if self.pool_len == 0 {
            return None;
        }
        let start = match self.active {
            // Rotate strictly after the currently active endpoint.
            Some(active) => (active + 1).rem_euclid(self.pool_len.max(1)),
            // Nothing committed yet: begin at the first alternate.
            None => 0,
        };
        let now = Instant::now();
        for offset in 0..self.pool_len {
            let idx = (start + offset) % self.pool_len;
            if !self.health[idx].in_cooldown(now) {
                return Some(idx);
            }
        }
        None
    }

    /// Commit sampling to pool index `idx`.
    pub fn set_active(&mut self, idx: usize) {
        assert!(idx < self.pool_len, "pool index out of range");
        self.active = Some(idx);
    }

    /// Record success for the currently active endpoint, resetting its
    /// failure counters.
    pub fn mark_active_success(&mut self) {
        if let Some(idx) = self.active {
            self.health[idx].record_success();
        }
    }

    /// Number of tracked pool endpoints.
    pub fn pool_len(&self) -> usize {
        self.pool_len
    }

    /// Whether any endpoint is still cooling down (diagnostics for
    /// pool-exhausted logging).
    pub fn any_in_cooldown(&self) -> bool {
        let now = Instant::now();
        self.health.iter().any(|h| h.in_cooldown(now))
    }

    /// Issue `GET {base_url}/models` with the endpoint's Authorization
    /// header and a short timeout, using the crate's shared HTTP client.
    ///
    /// Returns [`ProbeOutcome::Ok`] without I/O when probing is disabled
    /// via env (the caller then commits unprobed, preserving legacy
    /// behavior for opt-outs).
    pub async fn probe_endpoint(endpoint: &FailoverEndpoint) -> ProbeOutcome {
        if !probing_enabled() {
            return ProbeOutcome::Ok;
        }
        let client = match shared_http::client() {
            Ok(client) => client,
            Err(err) => {
                tracing::warn!(error = %err, "failed to build shared client for liveness probe");
                return ProbeOutcome::Unreachable;
            }
        };
        let mut builder = client.get(models_url(&endpoint.base_url));
        match endpoint.auth_scheme {
            AuthScheme::Bearer => {
                if let Some(key) = &endpoint.api_key {
                    builder = builder.bearer_auth(key);
                }
            }
            AuthScheme::XApiKey => {
                if let Some(key) = &endpoint.api_key {
                    builder = builder.header("x-api-key", key);
                }
            }
        }
        let response = match tokio::time::timeout(PROBE_TIMEOUT, builder.send()).await {
            Ok(Ok(response)) => response,
            Ok(Err(err)) => {
                tracing::debug!(error = %err, "failover probe transport error");
                return ProbeOutcome::Unreachable;
            }
            Err(_) => {
                tracing::debug!("failover probe timed out");
                return ProbeOutcome::Unreachable;
            }
        };
        match response.status().as_u16() {
            200 => ProbeOutcome::Ok,
            401 | 403 => {
                tracing::debug!(
                    status = response.status().as_u16(),
                    "failover probe reachable but unauthorized"
                );
                ProbeOutcome::ReachableButUnauthorized
            }
            status => {
                tracing::debug!(status, "failover probe got non-200");
                ProbeOutcome::Unreachable
            }
        }
    }
}

/// `{base}/models` with trailing-slash normalization; query params on the
/// base URL are preserved by `reqwest::Url`.
fn models_url(base_url: &str) -> String {
    format!("{}/models", base_url.trim_end_matches('/'))
}

/// Failover eligibility: whether this error is the kind a hop to another
/// endpoint could plausibly fix — rate limits, retryable 5xx / transport /
/// stream errors, and empty responses. Deliberately excludes everything the
/// retry classifier would not retry anyway (auth, vetoes, image-strip
/// cases, Fatals).
pub(crate) fn qualifies_for_failover(err: &xai_grok_sampling_types::SamplingError) -> bool {
    err.is_rate_limited() || err.is_retryable()
}

/// Consecutive failures on the active endpoint before a request hops to the
/// next pool entry. A single transient blip retries in-place — cheap and
/// prompt-cache-warm; only repeated failures indicate the endpoint is
/// actually degraded enough to justify the cache-breaking switch.
pub(crate) const FAILOVER_THRESHOLD: u32 = 2;

/// Whether the active endpoint has failed enough consecutive times to
/// justify hopping. `consecutive` is the CURRENT streak including this
/// latest failure.
pub(crate) fn threshold_reached(consecutive: u32) -> bool {
    consecutive >= FAILOVER_THRESHOLD
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{FailoverEndpoint, FailoverPool};
    use xai_grok_sampling_types::ApiBackend;

    fn endpoint(base_url: &str, model: &str) -> FailoverEndpoint {
        FailoverEndpoint {
            base_url: base_url.into(),
            model: model.into(),
            api_key: None,
            auth_scheme: AuthScheme::default(),
            api_backend: ApiBackend::default(),
            extra_headers: Default::default(),
            query_params: Default::default(),
        }
    }

    fn pool(n: usize) -> FailoverPool {
        FailoverPool {
            endpoints: (0..n)
                .map(|i| endpoint(&format!("https://alt{i}.example.com"), &format!("m{i}")))
                .collect(),
        }
    }

    #[test]
    fn empty_pool_has_no_candidates() {
        let state = FailoverState::new(&pool(0));
        assert_eq!(state.next_healthy(), None);
    }

    #[test]
    fn rotation_starts_at_first_alternate_then_advances() {
        let mut state = FailoverState::new(&pool(3));
        assert_eq!(state.next_healthy(), Some(0), "start at first alternate");
        state.set_active(0);
        assert_eq!(state.next_healthy(), Some(1));
        state.set_active(1);
        assert_eq!(state.next_healthy(), Some(2));
        state.set_active(2);
        // Wrap around: index 0 was never marked failed, so it is healthy again.
        assert_eq!(state.next_healthy(), Some(0));
    }

    #[test]
    fn single_endpoint_pool_wraps_back_to_itself_while_healthy() {
        let mut state = FailoverState::new(&pool(1));
        state.set_active(0);
        assert_eq!(state.next_healthy(), Some(0));
    }

    #[test]
    fn cooldown_skips_failed_endpoints() {
        let mut state = FailoverState::new(&pool(2));
        state.set_active(0);
        state.mark_active_failed(false); // 30s cooldown on pool index 0
        assert_eq!(state.next_healthy(), Some(1));
        state.set_active(1);
        state.mark_active_failed(true); // 60s cooldown on pool index 1
        assert_eq!(state.next_healthy(), None, "all candidates cooling");
        assert!(state.any_in_cooldown());
    }

    #[test]
    fn repeated_failure_doubles_cooldown_up_to_cap() {
        let mut health = EndpointHealth::default();
        let t0 = Instant::now();
        health.record_failure(EndpointErrorKind::ServerOrTransport, t0);
        let first = health.cooldown_until.unwrap().duration_since(t0);
        assert_eq!(first, Duration::from_secs(30));

        let t1 = Instant::now();
        health.record_failure(EndpointErrorKind::ServerOrTransport, t1);
        let second = health.cooldown_until.unwrap().duration_since(t1);
        assert_eq!(second, Duration::from_secs(60));

        // Many failures saturate at the cap.
        for _ in 0..12 {
            health.record_failure(EndpointErrorKind::RateLimited, Instant::now());
        }
        let capped = health
            .cooldown_until
            .unwrap()
            .duration_since(Instant::now());
        assert!(capped <= MAX_COOLDOWN, "cooldown must cap at 10 min");
        assert_eq!(health.consecutive_failures, 14);
    }

    #[test]
    fn rate_limited_cooldown_is_longer_than_generic() {
        let mut rl = EndpointHealth::default();
        rl.record_failure(EndpointErrorKind::RateLimited, Instant::now());
        let mut generic = EndpointHealth::default();
        generic.record_failure(EndpointErrorKind::ServerOrTransport, Instant::now());
        assert!(rl.cooldown_until.unwrap() > generic.cooldown_until.unwrap());
    }

    #[test]
    fn success_resets_state() {
        let mut state = FailoverState::new(&pool(2));
        state.set_active(0);
        state.mark_active_failed(false);
        assert_eq!(state.health[0].consecutive_failures, 1);
        state.mark_active_success();
        assert_eq!(
            state.health[0],
            EndpointHealth {
                consecutive_failures: 0,
                last_error_kind: None,
                cooldown_until: None,
                successes: 1,
            }
        );
    }

    #[tokio::test]
    async fn probe_unreachable_endpoint_fails_fast() {
        // Port 9 on localhost is reserved (discard protocol); connecting
        // fails fast without touching the network.
        let ep = endpoint("http://127.0.0.1:9", "m");
        let outcome = FailoverState::probe_endpoint(&ep).await;
        assert_eq!(outcome, ProbeOutcome::Unreachable);
    }

    #[test]
    fn models_url_normalizes_trailing_slash() {
        assert_eq!(
            models_url("https://x.example.com/v1/"),
            "https://x.example.com/v1/models"
        );
        assert_eq!(
            models_url("https://x.example.com/v1"),
            "https://x.example.com/v1/models"
        );
    }
}
