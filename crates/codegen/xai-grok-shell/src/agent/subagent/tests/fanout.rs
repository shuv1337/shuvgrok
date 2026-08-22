#![cfg_attr(rustfmt, rustfmt::skip)]
//! `[subagent_fanout]` parse + routing tests.
//!
//! Covers: lenient TOML parsing and warnings, catalog validation, the
//! provable-credential pool gate, per-spawn rotation, explicit-override
//! bypass for other models, inherit-parent matching, and failover_pool
//! staying `None` when disabled or single-entry.

use super::{reset_fanout_rotation_for_test, test_model_entry};
use super::super::{
    SubagentFanoutRuntime, SubagentSpawnContext, apply_subagent_fanout,
    resolve_effective_model_config, FanoutProvenance,
};
use crate::test_support::lsp_runtime::ctx_with_toggle;
use agent_client_protocol as acp;
use std::collections::HashMap;

fn entry_with_key(model_id: &str) -> crate::agent::config::ModelEntry {
    let mut info = test_model_entry(model_id).info;
    info.base_url = format!("https://{model_id}.example/v1");
    crate::agent::config::ModelEntry {
        info,
        api_key: Some(format!("{model_id}-key")),
        env_key: None,
        auth_provider: None,
        api_base_url: None,
    }
}

/// Same shape but NO credential: `own_credential()` must resolve to None so
/// the pool gate can exclude it. The base_url points at a non-xAI host so
/// the global-key fallback in `resolve_credentials` (which would hand the
/// session/global key to any endpoint) does not apply — the gate keys on
/// the entry's OWN credential only.
fn entry_without_key(model_id: &str) -> crate::agent::config::ModelEntry {
    let mut info = test_model_entry(model_id).info;
    info.base_url = format!("https://{model_id}.example/v1");
    crate::agent::config::ModelEntry {
        info,
        api_key: None,
        env_key: None,
        auth_provider: None,
        api_base_url: None,
    }
}

fn fanout_ctx(
    available_models: indexmap::IndexMap<String, crate::agent::config::ModelEntry>,
    fanout: Option<SubagentFanoutRuntime>,
    parent_model_id: &str,
) -> SubagentSpawnContext {
    let mut ctx = ctx_with_toggle(HashMap::new());
    ctx.available_models = available_models;
    ctx.fanout = fanout;
    ctx.model_id = acp::ModelId::new(parent_model_id);
    ctx.sampling_config.base_url = "https://parent.example/v1".into();
    ctx.sampling_config.model = "parent-routing-slug".into();
    ctx
}

fn models(ids: &[&str]) -> indexmap::IndexMap<String, crate::agent::config::ModelEntry> {
    ids.iter()
        .map(|id| (id.to_string(), entry_with_key(id)))
        .collect()
}

fn runtime(default_model: &str, pool: &[&str]) -> Option<SubagentFanoutRuntime> {
    Some(SubagentFanoutRuntime {
        default_model: default_model.to_string(),
        pool: pool.iter().map(|s| s.to_string()).collect(),
    })
}

// ---------------------------------------------------------------------------
// Config parsing ([subagent_fanout] section)
// ---------------------------------------------------------------------------

#[test]
fn fanout_config_parses_full_section() {
    let raw: toml::Value = toml::from_str(
        r#"
        [subagent_fanout]
        enabled = true
        default_model = "ox-alpha"
        pool = ["ox-alpha", "ox-alpha-zen", "ox-alpha-kilo"]
        "#,
    )
    .unwrap();
    let (fanout, warnings) = crate::agent::model_providers::parse_subagent_fanout(&raw);
    assert!(warnings.is_empty());
    let fanout = fanout.expect("section should parse");
    assert!(fanout.enabled);
    assert_eq!(fanout.default_model.as_deref(), Some("ox-alpha"));
    assert_eq!(fanout.pool.len(), 3);
}

#[test]
fn fanout_config_defaults_to_disabled_and_empty() {
    let raw: toml::Value = toml::from_str(
        r#"
        [subagent_fanout]
        "#,
    )
    .unwrap();
    let (fanout, warnings) = crate::agent::model_providers::parse_subagent_fanout(&raw);
    assert!(warnings.is_empty());
    let fanout = fanout.unwrap();
    assert!(!fanout.enabled);
    assert_eq!(fanout.default_model, None);
    assert!(fanout.pool.is_empty());
}

#[test]
fn fanout_config_absent_section_yields_none() {
    let raw: toml::Value = toml::from_str("enabled = true").unwrap();
    let (fanout, warnings) = crate::agent::model_providers::parse_subagent_fanout(&raw);
    assert!(fanout.is_none());
    assert!(warnings.is_empty());
}

#[test]
fn fanout_config_unknown_field_warns() {
    let raw: toml::Value = toml::from_str(
        r#"
        [subagent_fanout]
        enabled = true
        strategy = "round-robin"
        "#,
    )
    .unwrap();
    let (fanout, warnings) = crate::agent::model_providers::parse_subagent_fanout(&raw);
    // The section itself still parses; only the unknown key warns.
    assert!(fanout.is_some());
    assert_eq!(warnings.len(), 1);
    assert!(matches!(
        warnings[0].target,
        crate::agent::config_model_override_parse::WarningTarget::ConfigKey { .. }
    ));
    assert_eq!(
        warnings[0].kind,
        crate::agent::config_model_override_parse::ConfigWarningKind::UnknownField
    );
    assert!(warnings[0].reason.contains("unrecognized key; field ignored"));
}

#[test]
fn fanout_config_non_table_warns_and_is_ignored() {
    let raw: toml::Value = toml::from_str("subagent_fanout = true").unwrap();
    let (fanout, warnings) = crate::agent::model_providers::parse_subagent_fanout(&raw);
    assert!(fanout.is_none());
    assert_eq!(warnings.len(), 1);
    assert_eq!(
        warnings[0].kind,
        crate::agent::config_model_override_parse::ConfigWarningKind::NotATable
    );
}

#[test]
fn fanout_config_bad_entry_type_skips_section_with_warning() {
    let raw: toml::Value = toml::from_str(
        r#"
        [subagent_fanout]
        enabled = "yes"
        "#,
    )
    .unwrap();
    let (fanout, warnings) = crate::agent::model_providers::parse_subagent_fanout(&raw);
    assert!(fanout.is_none(), "unparseable section is dropped");
    assert_eq!(warnings.len(), 1);
    assert_eq!(
        warnings[0].kind,
        crate::agent::config_model_override_parse::ConfigWarningKind::InvalidValue
    );
}

#[test]
fn fanout_config_duplicate_pool_ids_warn_once_keep_first_order() {
    let raw: toml::Value = toml::from_str(
        r#"
        [subagent_fanout]
        enabled = true
        pool = ["a", "b", "a", "b", "a"]
        "#,
    )
    .unwrap();
    let (fanout, warnings) = crate::agent::model_providers::parse_subagent_fanout(&raw);
    let fanout = fanout.unwrap();
    assert_eq!(fanout.pool, vec!["a".to_string(), "b".to_string()]);
    assert_eq!(
        warnings.len(),
        3,
        "one warning per duplicate OCCURRENCE (a, b, a)"
    );
    for w in &warnings {
        assert_eq!(
            w.kind,
            crate::agent::config_model_override_parse::ConfigWarningKind::ConflictingFields
        );
    }
}

#[test]
fn fanout_config_missing_catalog_id_warns_by_name_via_new_from_toml_cfg() {
    let raw: toml::Value = toml::from_str(
        r#"
        [subagent_fanout]
        enabled = true
        default_model = "ox-alpha"
        pool = ["ox-alpha", "ghost-model"]

        [model.ox-alpha]
        model = "vendor/ox-alpha"
        base_url = "https://openrouter.example/v1"
        env_key = "SOME_KEY"
        context_window = 200_000
        "#,
    )
    .unwrap();
    let cfg = crate::agent::config::Config::new_from_toml_cfg(&raw)
        .expect("config must still load");
    let missing: Vec<_> = cfg
        .config_warnings
        .iter()
        .filter(|w| {
            w.kind == crate::agent::config_model_override_parse::ConfigWarningKind::InvalidValue
                && w.reason.contains("ghost-model")
        })
        .collect();
    assert_eq!(missing.len(), 1, "exactly one warning naming ghost-model");
    let fanout = cfg.subagent_fanout.expect("section retained");
    assert_eq!(fanout.pool, vec!["ox-alpha", "ghost-model"]);
}

#[test]
fn fanout_config_valid_ids_produce_no_validation_warning() {
    let raw: toml::Value = toml::from_str(
        r#"
        [subagent_fanout]
        enabled = true
        pool = ["m1", "m2"]

        [model.m1]
        model = "a"
        context_window = 1000

        [model.m2]
        model = "b"
        context_window = 1000
        "#,
    )
    .unwrap();
    let cfg =
        crate::agent::config::Config::new_from_toml_cfg(&raw).expect("config must still load");
    assert!(
        !cfg.config_warnings
            .iter()
            .any(|w| w.reason.contains("subagent_fanout") || matches!(&w.target,
                crate::agent::config_model_override_parse::WarningTarget::ConfigKey { path }
                    if path.starts_with("subagent_fanout.pool"))),
        "no pool-id warnings expected, got {:?}",
        cfg.config_warnings
    );
}

#[test]
fn fanout_runtime_from_config_requires_enabled_default_and_nonempty_pool() {
    use crate::agent::config::SubagentFanoutConfig;
    assert_eq!(SubagentFanoutRuntime::from_config(None), None);
    assert_eq!(
        SubagentFanoutRuntime::from_config(Some(&SubagentFanoutConfig {
            enabled: false,
            default_model: Some("ox-alpha".into()),
            pool: vec!["ox-alpha".into()],
        })),
        None,
        "disabled ⇒ None"
    );
    assert_eq!(
        SubagentFanoutRuntime::from_config(Some(&SubagentFanoutConfig {
            enabled: true,
            default_model: Some("ox-alpha".into()),
            pool: Vec::new(),
        })),
        None,
        "empty pool ⇒ Nothing to rotate over"
    );
    assert_eq!(
        SubagentFanoutRuntime::from_config(Some(&SubagentFanoutConfig {
            enabled: true,
            default_model: None,
            pool: vec!["ox-alpha".into()],
        })),
        None,
        "no default id ⇒ nothing to match against"
    );
    assert_eq!(
        SubagentFanoutRuntime::from_config(Some(&SubagentFanoutConfig {
            enabled: true,
            default_model: Some("ox-alpha".into()),
            pool: vec!["ox-alpha".into()],
        })),
        runtime("ox-alpha", &["ox-alpha"]),
    );
}

// ---------------------------------------------------------------------------
// Routing: apply_subagent_fanout / resolve_effective_model_config
// ---------------------------------------------------------------------------

/// A resolved explicit override for the fanout default gets the rotated pool:
/// active config overwritten from pool[0], remaining entries as alternates.
/// Serial: the rotation counter is process-global, so the exact indices this
/// asserts on require exclusive runs (other fanout tests also consume ticks).
#[test]
#[serial_test::serial]
fn fanout_applies_to_explicit_default_override_with_rotation() {
    // Reset the counter so the first spawn here deterministically starts at 0.
    reset_fanout_rotation_for_test();
    let ctx = fanout_ctx(
        models(&["ox-alpha", "zen", "kilo"]),
        runtime("ox-alpha", &["ox-alpha", "zen", "kilo"]),
        "ox-alpha",
    );
    let resolved = super::super::resolve_model_override_to_config("ox-alpha", &ctx).unwrap();
    let (config, model_id) =
        apply_subagent_fanout(&resolved, FanoutProvenance::Explicit, &ctx);
    assert_eq!(model_id.0.as_ref(), "ox-alpha");
    let pool = config.failover_pool.expect("pool attached");
    assert_eq!(pool.endpoints.len(), 3, "two alternates + active");
    // Active must equal pool[0] post-rotation.
    assert_eq!(config.base_url, pool.endpoints[0].base_url);
    assert_eq!(config.model, pool.endpoints[0].model);
    assert_eq!(config.api_key, pool.endpoints[0].api_key);
    // Rotation counter hands out distinct starts across spawns.
    let mut starts = std::collections::HashSet::new();
    for _ in 1..=3usize {
        let (next_config, _) =
            apply_subagent_fanout(&resolved, FanoutProvenance::Explicit, &ctx);
        let next_pool = next_config.failover_pool.as_ref().unwrap();
        // Active must equal pool[0] on every spawn.
        assert_eq!(next_config.base_url, next_pool.endpoints[0].base_url);
        starts.insert(next_pool.endpoints[0].base_url.clone());
    }
    assert_eq!(
        starts.len(),
        3,
        "three spawns must land on three DIFFERENT starting endpoints"
    );
}

/// Rotation reorders the whole vec, not just the head.
#[test]
fn fanout_rotation_reorders_endpoints_vec() {
    let ctx = fanout_ctx(
        models(&["a", "b"]),
        runtime("a", &["a", "b"]),
        "a",
    );
    let resolved = super::super::resolve_model_override_to_config("a", &ctx).unwrap();
    let (config, _) = apply_subagent_fanout(&resolved, FanoutProvenance::Explicit, &ctx);
    let pool = config.failover_pool.unwrap();
    assert_eq!(pool.endpoints.len(), 2);
    assert_eq!(pool.endpoints[0].model, config.model);
}

/// An explicit override for a DIFFERENT model never gains a pool — even
/// though that model also sits in the fanout pool.
#[test]
fn fanout_bypasses_explicit_override_for_other_model() {
    let ctx = fanout_ctx(
        models(&["ox-alpha", "zen"]),
        runtime("ox-alpha", &["ox-alpha", "zen"]),
        "ox-alpha",
    );
    let resolved = super::super::resolve_model_override_to_config("zen", &ctx).unwrap();
    let untouched = resolved.clone();
    let (config, model_id) =
        apply_subagent_fanout(&resolved, FanoutProvenance::Explicit, &ctx);
    assert_eq!(model_id.0.as_ref(), "zen");
    assert!(config.failover_pool.is_none());
    assert_eq!(
        serde_json::to_string(&config.base_url).unwrap(),
        serde_json::to_string(&untouched.0.base_url).unwrap()
    );
    assert_eq!(config.base_url, untouched.0.base_url);
    assert_eq!(config.api_key, untouched.0.api_key);
}

/// Inherit-parent applies fanout only when the PARENT's catalog id equals
/// the fanout default; the inherited config's connection fields are then
/// replaced by pool[0].
#[tokio::test]
async fn fanout_applies_on_inherit_parent_when_parent_is_default() {
    let ctx = fanout_ctx(
        models(&["ox-alpha", "zen"]),
        runtime("ox-alpha", &["ox-alpha", "zen"]),
        "ox-alpha",
    );
    let parent = super::super::read_parent_sampling_config(&ctx).await;
    let (config, model_id) =
        apply_subagent_fanout(&parent, FanoutProvenance::InheritedParent, &ctx);
    assert_eq!(model_id.0.as_ref(), "ox-alpha");
    assert!(config.failover_pool.is_some(), "parent on default ⇒ pooled");
}

#[tokio::test]
async fn fanout_bypassed_on_inherit_parent_when_parent_is_other_model() {
    let ctx = fanout_ctx(
        models(&["ox-alpha", "other"]),
        runtime("ox-alpha", &["ox-alpha"]),
        "other",
    );
    let parent = super::super::read_parent_sampling_config(&ctx).await;
    let (config, _) = apply_subagent_fanout(&parent, FanoutProvenance::InheritedParent, &ctx);
    assert!(config.failover_pool.is_none());
    assert_eq!(config.base_url, "https://parent.example/v1");
}

/// Pool membership requires PROVABLE credentials at build time: an entry
/// with no own credential is excluded (and logged), not assumed available.
/// Two credentialed entries surround the keyless one so the survivor count
/// pins the exclusion exactly.
#[test]
#[serial_test::serial]
fn fanout_excludes_entries_without_resolvable_credentials() {
    let _no_global = xai_grok_test_support::EnvGuard::unset(
        crate::agent::auth_method::XAI_API_KEY_ENV_VAR,
    );
    let _no_legacy = xai_grok_test_support::EnvGuard::unset(
        crate::agent::auth_method::LEGACY_XAI_API_KEY_ENV_VAR,
    );
    reset_fanout_rotation_for_test();
    let mut available = models(&["ox-alpha", "zen"]);
    // Keyless entry: no api_key/env_key/provider, non-xAI host.
    available.insert("keyless".to_string(), entry_without_key("keyless"));
    let ctx = fanout_ctx(
        available,
        runtime("ox-alpha", &["ox-alpha", "keyless", "zen"]),
        "ox-alpha",
    );
    let resolved = super::super::resolve_model_override_to_config("ox-alpha", &ctx).unwrap();
    assert!(
        resolved.0.api_key.is_some(),
        "precondition: the active entry itself carries a static key"
    );
    let (config, _) = apply_subagent_fanout(&resolved, FanoutProvenance::Explicit, &ctx);
    let pool = config
        .failover_pool
        .expect("two credentialed entries ⇒ one alternate");
    assert_eq!(
        pool.endpoints.len(),
        2,
        "exactly the credentialed entries survive"
    );
    for ep in &pool.endpoints {
        assert_ne!(ep.model, "keyless");
    }
}

/// Unknown pool ids are skipped (validation normally warns at parse time).
#[test]
fn fanout_skips_unknown_pool_ids() {
    let ctx = fanout_ctx(
        models(&["ox-alpha", "real"]),
        runtime("ox-alpha", &["ox-alpha", "ghost", "real"]),
        "ox-alpha",
    );
    let resolved = super::super::resolve_model_override_to_config("ox-alpha", &ctx).unwrap();
    let (config, _) = apply_subagent_fanout(&resolved, FanoutProvenance::Explicit, &ctx);
    let pool = config.failover_pool.expect("credentialed entries remain");
    assert_eq!(pool.endpoints.len(), 2);
    for ep in &pool.endpoints {
        assert_ne!(ep.model, "ghost");
    }
}

/// Every entry unprovable ⇒ no pool at all; base resolution passes through.
#[test]
fn fanout_no_credentialed_entries_leaves_resolution_untouched() {
    let mut available = indexmap::IndexMap::new();
    let mut bare = test_model_entry("bare");
    bare.info.base_url = "https://bare.example/v1".into();
    available.insert("bare".to_string(), bare.clone());
    let mut alpha = test_model_entry("ox-alpha");
    alpha.info.base_url = "https://alpha.example/v1".into();
    available.insert("ox-alpha".to_string(), alpha);
    let ctx = fanout_ctx(available, runtime("ox-alpha", &["bare"]), "ox-alpha");
    let resolved = super::super::resolve_model_override_to_config("ox-alpha", &ctx).unwrap();
    let before = resolved.0.clone();
    let (config, _) = apply_subagent_fanout(&resolved, FanoutProvenance::Explicit, &ctx);
    assert!(config.failover_pool.is_none());
    // No credentialed entry existed, so the ACTIVE config keeps its own
    // resolution (the ox-alpha entry here is credential-less too).
    assert_eq!(config.base_url, before.base_url);
}

/// Disabled/absent fanout: pure pass-through, byte-for-byte.
#[test]
fn fanout_absent_is_pass_through() {
    let ctx = fanout_ctx(models(&["ox-alpha"]), None, "ox-alpha");
    let resolved = super::super::resolve_model_override_to_config("ox-alpha", &ctx).unwrap();
    let before = resolved.clone();
    let (config, _) = apply_subagent_fanout(&resolved, FanoutProvenance::Explicit, &ctx);
    assert!(config.failover_pool.is_none());
    assert_eq!(config.base_url, before.0.base_url);
}

/// Single-entry pool after gating: active endpoint rotates onto it but
/// `failover_pool` stays None (nothing to fail over TO).
#[test]
fn fanout_single_entry_sets_active_but_no_pool() {
    let ctx = fanout_ctx(
        models(&["ox-alpha", "zen"]),
        runtime("ox-alpha", &["zen"]),
        "ox-alpha",
    );
    let resolved = super::super::resolve_model_override_to_config("ox-alpha", &ctx).unwrap();
    let (config, _) = apply_subagent_fanout(&resolved, FanoutProvenance::Explicit, &ctx);
    assert!(
        config.failover_pool.is_none(),
        "single entry ⇒ no alternate ⇒ no FailoverPool"
    );
    assert_eq!(config.base_url, "https://zen.example/v1");
    assert_eq!(config.api_key.as_deref(), Some("zen-key"));
}

/// End-to-end precedence: an explicit runtime override for a NON-default
/// model wins and stays unpooled; the same call with the default pools.
#[tokio::test]
async fn resolve_effective_model_config_honors_runtime_override_bypass() {
    let ctx = fanout_ctx(
        models(&["ox-alpha", "zen"]),
        runtime("ox-alpha", &["ox-alpha", "zen"]),
        "ox-alpha",
    );
    let definition = xai_grok_agent::config::ModelOverride::Inherit;
    let (config, model_id) = resolve_effective_model_config(
        Some("zen"),
        "general-purpose",
        &definition,
        &ctx,
    )
    .await;
    assert_eq!(model_id.0.as_ref(), "zen");
    assert!(config.failover_pool.is_none(), "non-default override bypassed");

    let (config, model_id) = resolve_effective_model_config(
        Some("ox-alpha"),
        "general-purpose",
        &definition,
        &ctx,
    )
    .await;
    assert_eq!(model_id.0.as_ref(), "ox-alpha");
    assert!(config.failover_pool.is_some(), "default override pooled");
}

/// Resume pinning bypasses fanout entirely: the handle_request.rs pin block
/// runs AFTER resolve_effective_model_config and assigns the raw
/// `resolve_model_override_to_config` output, so a resumed child pinned to
/// any model — including the fanout default — carries NO pool.
#[test]
fn resume_pin_bypass_keeps_pinned_child_unpooled() {
    let ctx = fanout_ctx(
        models(&["ox-alpha", "zen"]),
        runtime("ox-alpha", &["ox-alpha", "zen"]),
        "ox-alpha",
    );
    let pinned = super::super::resolve_model_override_to_config("ox-alpha", &ctx).unwrap();
    // This is exactly what handle_request.rs does on the pin path
    // (effective_sampling_config = resolved.0) — no apply_subagent_fanout.
    assert!(pinned.0.failover_pool.is_none());
}
