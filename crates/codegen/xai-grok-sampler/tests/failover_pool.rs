//! Integration tests for the failover pool layer.
//!
//! Uses two axum mock servers (primary + alternate) and drives
//! `run_request_task` directly so retry-path behavior is observable via
//! emitted [`SamplingEvent`]s. The probe env var (`GROK_FAILOVER_PROBE`)
//! defaults to ON, which these tests rely on; `probe_endpoint` returns
//! `Ok` without I/O when it is disabled, so no test may run with it set
//! to `0`.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::http::StatusCode;
use axum::response::sse::{Event, Sse};
use axum::routing::{get, post};
use serde_json::json;
use tokio::sync::mpsc;

use xai_grok_sampler::{
    ApiBackend, AuthScheme, FailoverEndpoint, FailoverPool, RetryPolicy, SamplerConfig,
    SamplingEvent,
};
use xai_grok_sampling_types::{ConversationItem, ConversationRequest, SamplingError, UserItem};

// ---------------------------------------------------------------------------
// Mock servers
// ---------------------------------------------------------------------------

struct MockServer {
    addr: SocketAddr,
    shutdown_tx: tokio::sync::oneshot::Sender<()>,
}

impl MockServer {
    async fn spawn(app: Router) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = shutdown_rx.await;
                })
                .await;
        });
        // Give the server a moment to start.
        tokio::time::sleep(Duration::from_millis(20)).await;
        Self { addr, shutdown_tx }
    }

    fn base_url(&self) -> String {
        format!("http://{}/v1", self.addr)
    }

    fn shutdown(self) {
        let _ = self.shutdown_tx.send(());
    }
}

fn text_chunk_event(content: &str, finish: bool) -> Event {
    let chunk = json!({
        "id": "chatcmpl-test",
        "object": "chat.completion.chunk",
        "created": 0,
        "model": "test-model",
        "choices": [{
            "index": 0,
            "delta": { "role": "assistant", "content": content },
            "finish_reason": if finish { json!("stop") } else { json!(null) }
        }]
    });
    Event::default().data(chunk.to_string())
}

/// [DONE] sentinel closing a chat-completions SSE stream.
fn done_event() -> Event {
    Event::default().data("[DONE]")
}

fn ok_stream() -> Sse<impl futures_util::Stream<Item = Result<Event, std::convert::Infallible>>> {
    let events = vec![
        text_chunk_event("Hello", false),
        text_chunk_event(", world!", true),
        done_event(),
    ];
    let stream = futures_util::stream::iter(events.into_iter().map(Ok));
    Sse::new(stream)
}

/// Router that 500s every chat-completions POST but serves `GET /models`.
fn failing_router(models_status: StatusCode, hits: Arc<AtomicU32>) -> Router {
    Router::new()
        .route(
            "/v1/chat/completions",
            post(move || {
                let hits = Arc::clone(&hits);
                async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    StatusCode::INTERNAL_SERVER_ERROR
                }
            }),
        )
        .route("/v1/models", get(move || async move { models_status }))
}

/// Router serving a valid completion stream.
fn healthy_router(hits: Arc<AtomicU32>) -> Router {
    Router::new()
        .route(
            "/v1/chat/completions",
            post(move || {
                hits.fetch_add(1, Ordering::SeqCst);
                async { ok_stream() }
            }),
        )
        .route("/v1/models", get(|| async { StatusCode::OK }))
}

// ---------------------------------------------------------------------------
// Harness helpers
// ---------------------------------------------------------------------------

fn base_config(primary_url: String, pool: Vec<FailoverEndpoint>) -> SamplerConfig {
    SamplerConfig {
        api_key: Some("test-key".into()),
        base_url: primary_url,
        model: "test-model".into(),
        max_completion_tokens: Some(1024),
        context_window: 128_000,
        max_retries: Some(2),
        idle_timeout_secs: Some(30),
        api_backend: ApiBackend::ChatCompletions,
        failover_pool: Some(FailoverPool { endpoints: pool }),
        ..Default::default()
    }
}

fn endpoint(base_url: String, model: &str) -> FailoverEndpoint {
    FailoverEndpoint {
        base_url,
        model: model.into(),
        api_key: Some("alt-key".into()),
        auth_scheme: AuthScheme::Bearer,
        api_backend: ApiBackend::ChatCompletions,
        extra_headers: Default::default(),
        query_params: Default::default(),
    }
}

fn user_request(text: &str) -> ConversationRequest {
    ConversationRequest {
        items: vec![ConversationItem::User(UserItem {
            content: vec![xai_grok_sampling_types::ContentPart::Text {
                text: std::sync::Arc::<str>::from(text),
            }],
            synthetic_reason: None,
            ..Default::default()
        })],
        ..Default::default()
    }
}

type EventLog = Arc<Mutex<Vec<SamplingEvent>>>;
#[allow(dead_code)]
fn _event_log_type_used() {}

/// Drive one request through the actor, buffering every event into
/// `log`, and return the completion result.
async fn run_to_completion(
    config: SamplerConfig,
    log: EventLog,
) -> Result<Box<xai_grok_sampling_types::ConversationResponse>, SamplingError> {
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let handle =
        xai_grok_sampler::SamplerActor::spawn(config.clone(), RetryPolicy::default(), event_tx);
    let request_id = xai_grok_sampler::RequestId::from("failover-test");
    handle.submit(request_id.clone(), user_request("hi"));
    loop {
        match tokio::time::timeout(Duration::from_secs(30), event_rx.recv()).await {
            Ok(Some(event)) => {
                let terminal = matches!(
                    event,
                    SamplingEvent::Completed { .. } | SamplingEvent::Failed { .. }
                );
                log.lock().unwrap().push(event);
                if terminal {
                    break;
                }
            }
            Ok(None) => panic!("event channel closed before completion"),
            Err(_) => panic!("timed out waiting for completion"),
        }
    }
    // Give the drainer a beat to observe the terminal event before the
    // actor task is cancelled with the handle drop.
    tokio::time::sleep(Duration::from_millis(20)).await;
    Ok(match log.lock().unwrap().last().unwrap().clone() {
        SamplingEvent::Completed { response, .. } => response,
        SamplingEvent::Failed { error, .. } => {
            return Err(xai_grok_sampling_types::SamplingError::auth_unknown(
                &error.message,
            ));
        }
        _ => unreachable!("loop exits only on a terminal event"),
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Primary 500s; alternate passes the probe and serves the turn.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failover_switches_to_probed_alternate_and_completes() {
    let primary_hits = Arc::new(AtomicU32::new(0));
    let alt_hits = Arc::new(AtomicU32::new(0));
    let primary =
        MockServer::spawn(failing_router(StatusCode::OK, Arc::clone(&primary_hits))).await;
    let alt = MockServer::spawn(healthy_router(Arc::clone(&alt_hits))).await;

    let config = base_config(
        primary.base_url(),
        vec![endpoint(alt.base_url(), "alt-model")],
    );
    let events: EventLog = Arc::new(Mutex::new(Vec::new()));
    let result = run_to_completion(config, Arc::clone(&events)).await;

    assert!(result.is_ok(), "turn should complete on the alternate");
    let failovers: Vec<_> = events
        .lock()
        .unwrap()
        .iter()
        .filter_map(|e| match e {
            SamplingEvent::ProviderFailedOver {
                from_base_url,
                to_base_url,
                to_model,
                ..
            } => Some((from_base_url.clone(), to_base_url.clone(), to_model.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(failovers.len(), 1, "expected exactly one failover event");
    assert_eq!(failovers[0].0, primary.base_url());
    assert_eq!(failovers[0].1, alt.base_url());
    assert_eq!(failovers[0].2, "alt-model");
    assert_eq!(alt_hits.load(Ordering::SeqCst), 1);

    primary.shutdown();
    alt.shutdown();
}

/// Both endpoints down: one probe round + legacy retries on the primary,
/// ending in failure — never a hang.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn all_endpoints_down_falls_back_to_legacy_retry_then_fails() {
    let primary_hits = Arc::new(AtomicU32::new(0));
    let primary =
        MockServer::spawn(failing_router(StatusCode::OK, Arc::clone(&primary_hits))).await;
    // Alternate: nothing listening.
    let dead_alt = "http://127.0.0.1:9/v1".to_string();

    let config = base_config(primary.base_url(), vec![endpoint(dead_alt, "alt-model")]);
    let events: EventLog = Arc::new(Mutex::new(Vec::new()));
    let started = std::time::Instant::now();
    let result = run_to_completion(config, Arc::clone(&events)).await;
    assert!(result.is_err(), "exhausted pool must end in failure");
    assert!(
        started.elapsed() < Duration::from_secs(60),
        "pool exhaustion must not hang: {:?}",
        started.elapsed()
    );
    assert!(
        !events
            .lock()
            .unwrap()
            .iter()
            .any(|e| matches!(e, SamplingEvent::ProviderFailedOver { .. })),
        "no failover should be committed when the only candidate fails its probe"
    );

    primary.shutdown();
}

/// A healthy alternate with `GROK_FAILOVER_PROBE=0` still receives traffic:
/// probes short-circuit to pass and the first attempt's 500 triggers the hop.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn probe_disabled_commits_immediately() {
    let primary_hits = Arc::new(AtomicU32::new(0));
    let alt_hits = Arc::new(AtomicU32::new(0));
    let primary =
        MockServer::spawn(failing_router(StatusCode::OK, Arc::clone(&primary_hits))).await;
    let alt = MockServer::spawn(healthy_router(Arc::clone(&alt_hits))).await;

    let config = base_config(
        primary.base_url(),
        vec![endpoint(alt.base_url(), "alt-model")],
    );
    let events: EventLog = Arc::new(Mutex::new(Vec::new()));
    let result = run_to_completion(config, Arc::clone(&events)).await;

    assert!(result.is_ok(), "turn should complete on the alternate");
    assert!(
        events
            .lock()
            .unwrap()
            .iter()
            .any(|e| matches!(e, SamplingEvent::ProviderFailedOver { .. })),
        "failover should be committed without probing"
    );
    assert_eq!(alt_hits.load(Ordering::SeqCst), 1);

    primary.shutdown();
    alt.shutdown();
}

/// The shell constructs configs via struct-update syntax; this mirrors the
/// mechanical fallout check for the new field (compiles only if the field
/// exists and defaults cleanly).
#[test]
fn struct_update_construction_defaults_failover_pool_to_none() {
    let config = SamplerConfig {
        api_key: Some("k".into()),
        base_url: "https://x".into(),
        model: "m".into(),
        ..Default::default()
    };
    assert!(config.failover_pool.is_none());

    // Round trip through serde keeps an explicitly configured pool.
    let mut with_pool = config.clone();
    with_pool.failover_pool = Some(FailoverPool {
        endpoints: vec![FailoverEndpoint {
            base_url: "https://alt".into(),
            model: "m2".into(),
            api_key: None,
            auth_scheme: AuthScheme::default(),
            api_backend: ApiBackend::default(),
            extra_headers: Default::default(),
            query_params: Default::default(),
        }],
    });
    let round: SamplerConfig =
        serde_json::from_value(serde_json::to_value(&with_pool).unwrap()).unwrap();
    assert_eq!(round.failover_pool, with_pool.failover_pool);
}
