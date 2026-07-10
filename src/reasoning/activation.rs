//! Reasoning activation policy — ARB "Eje A" (Issue #90-B), extended by #58/#101/#57.
//!
//! Replaces the hardcoded `has_thinking = true` + fixed `chat_template_kwargs` with a
//! per-model policy that injects the intent "reason at maximum" translated to the mechanism
//! the upstream AND the specific model accepts. Safety invariants:
//!   - `chat_template_kwargs` (`enable_thinking`) is emitted ONLY for NIM models that
//!     support it (ChatTemplate mechanism) — never for Mistral/Devstral/LLaMA (#101).
//!   - `thinking` (`{type:"enabled", budget_tokens:N}`) is emitted ONLY for Anthropic-shaped
//!     upstreams (AnthropicApi mechanism) — closes #57.
//!   - Models on the DISABLE_THINKING_MODELS denylist get `ThinkingMechanism::None` — no
//!     injection at all.
//!
//! The per-model auto-probe (`ReasoningProfile`) that refines the mechanism per NIM
//! model is a documented follow-up (Phase 4); the config-driven activation is safe
//! without it.

use crate::config::{Config, ThinkingMechanism, UpstreamType};

/// Resolved reasoning activation for one request.
#[derive(Debug, Clone, PartialEq)]
pub struct Activation {
    /// Whether to route to the reasoning model and request thinking.
    pub has_thinking: bool,
    /// `chat_template_kwargs` to attach (NIM ChatTemplate models only), or `None`.
    pub chat_template_kwargs: Option<serde_json::Value>,
    /// Anthropic-API `thinking` payload (AnthropicApi mechanism only), or `None`.
    pub thinking: Option<serde_json::Value>,
}

/// Default budget_tokens when no config or client value is available.
/// Uses 80% of a typical max_tokens to leave room for actual output.
const DEFAULT_THINKING_BUDGET_TOKENS: u32 = 10000;

/// Resolve the activation for a specific target model under the `global_max` policy.
///
/// `target_model` is the resolved upstream model id (e.g. "deepseek-r1",
/// "mistralai/mistral-small-4-119b-2603"), NOT the Claude model id.
/// `upstream_type` is the resolved type for the upstream this request will hit.
/// `config` provides the denylist, per-route overrides, and budget_tokens config.
/// `route_mechanism` is the optional per-route ThinkingMechanism from MODEL_MAP 3rd segment.
/// `client_thinking` is the client's own `req.extra["thinking"]` if present.
/// `max_tokens` is the request's max_tokens, needed for budget_tokens clamping.
pub fn activate(
    upstream_type: UpstreamType,
    target_model: &str,
    config: &Config,
    route_mechanism: Option<ThinkingMechanism>,
    client_thinking: Option<&serde_json::Value>,
    max_tokens: u32,
) -> Activation {
    let mechanism = config.resolve_thinking_mechanism(target_model, upstream_type, route_mechanism);

    let has_thinking = mechanism != ThinkingMechanism::None;

    let chat_template_kwargs = if mechanism == ThinkingMechanism::ChatTemplate {
        Some(serde_json::json!({ "enable_thinking": true, "clear_thinking": false }))
    } else {
        None
    };

    let thinking = if mechanism == ThinkingMechanism::AnthropicApi {
        Some(build_thinking_payload(client_thinking, config, max_tokens))
    } else {
        None
    };

    Activation { has_thinking, chat_template_kwargs, thinking }
}

/// Build the `thinking: {type: "enabled", budget_tokens: N}` payload.
/// Budget resolution: (1) client's own budget_tokens, (2) THINKING_BUDGET_TOKENS config,
/// (3) hardcoded default. Clamped to be strictly less than max_tokens.
fn build_thinking_payload(
    client_thinking: Option<&serde_json::Value>,
    config: &Config,
    max_tokens: u32,
) -> serde_json::Value {
    // Try client's own budget_tokens first
    let client_budget = client_thinking
        .and_then(|t| t.get("budget_tokens"))
        .and_then(|b| b.as_u64())
        .map(|b| b as u32);

    let raw_budget =
        client_budget.or(config.thinking_budget_tokens).unwrap_or(DEFAULT_THINKING_BUDGET_TOKENS);

    // Clamp: budget_tokens must be strictly less than max_tokens
    let budget = if max_tokens > 1 { raw_budget.min(max_tokens - 1) } else { 0 };

    serde_json::json!({
        "type": "enabled",
        "budget_tokens": budget
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// Build a minimal Config for testing.
    fn test_config() -> Config {
        Config {
            port: 8315,
            bind_addr: "127.0.0.1".to_string(),
            base_url: "http://localhost:11434".to_string(),
            api_key: Some("test".to_string()),
            reasoning_model: None,
            completion_model: None,
            debug: false,
            verbose: false,
            web_fetch_enabled: false,
            web_fetch_max_retries: 0,
            web_fetch_timeout_secs: 0,
            upstreams: HashMap::new(),
            model_map: HashMap::new(),
            max_concurrent_per_model: 5,
            permit_timeout_secs: 180,
            max_queue_depth: 20,
            upstream_type: UpstreamType::NIM,
            cb_enabled: false,
            cb_threshold: 10,
            cb_recovery_secs: 60,
            cc_model_context_windows: HashMap::new(),
            telemetry_enabled: false,
            telemetry_beacon_url: None,
            beacon_auth_token: None,
            telemetry_dir: String::new(),
            telemetry_db_path: String::new(),
            telemetry_retention_days: 30,
            telemetry_secret_path: String::new(),
            telemetry_disabled_reason: None,
            config_path: None,
            env_file_path: None,
            disable_health_check: false,
            disable_thinking_models: Vec::new(),
            thinking_budget_tokens: None,
        }
    }

    // =======================================================================
    // Original 3 tests — updated signatures, same semantics
    // =======================================================================

    #[test]
    fn nim_gets_enable_thinking_kwargs() {
        let cfg = test_config();
        let a = activate(UpstreamType::NIM, "z-ai/glm5", &cfg, None, None, 16000);
        assert!(a.has_thinking);
        let kw = a.chat_template_kwargs.expect("NIM must receive kwargs");
        assert_eq!(kw["enable_thinking"], true);
        assert_eq!(kw["clear_thinking"], false);
        // NIM should NOT get Anthropic thinking
        assert!(a.thinking.is_none());
    }

    #[test]
    fn never_sends_kwargs_to_non_nim() {
        let cfg = test_config();
        // Safety invariant: Anthropic/OpenAI/OpenRouter must never receive
        // chat_template_kwargs (they would reject the field).
        for ut in [UpstreamType::OpenAI, UpstreamType::OpenRouter] {
            let a = activate(ut, "some-model", &cfg, None, None, 16000);
            assert_eq!(a.chat_template_kwargs, None);
        }
        // Anthropic gets thinking instead, not kwargs
        let a = activate(UpstreamType::Anthropic, "claude-opus", &cfg, None, None, 16000);
        assert_eq!(a.chat_template_kwargs, None);
    }

    #[test]
    fn reasoning_independent_of_claude_model_id() {
        // global_max forces reasoning regardless of route; `activate` depends only on the
        // upstream type + target model, never on the Claude model id.
        let cfg = test_config();
        assert!(activate(UpstreamType::NIM, "deepseek-r1", &cfg, None, None, 16000).has_thinking);
        assert!(
            activate(UpstreamType::Anthropic, "claude-opus", &cfg, None, None, 16000).has_thinking
        );
    }

    // =======================================================================
    // New tests for #101 — denylist
    // =======================================================================

    #[test]
    fn denylist_blocks_mistral_kwargs() {
        let mut cfg = test_config();
        cfg.disable_thinking_models = vec!["mistral".to_string()];
        let a = activate(
            UpstreamType::NIM,
            "mistralai/mistral-small-4-119b-2603",
            &cfg,
            None,
            None,
            16000,
        );
        assert!(!a.has_thinking);
        assert!(a.chat_template_kwargs.is_none());
        assert!(a.thinking.is_none());
    }

    #[test]
    fn denylist_does_not_block_thinking_models() {
        let mut cfg = test_config();
        cfg.disable_thinking_models = vec!["mistral".to_string()];
        // GLM should still get kwargs
        let a = activate(UpstreamType::NIM, "z-ai/glm5", &cfg, None, None, 16000);
        assert!(a.has_thinking);
        assert!(a.chat_template_kwargs.is_some());
    }

    #[test]
    fn denylist_case_insensitive() {
        let mut cfg = test_config();
        cfg.disable_thinking_models = vec!["mistral".to_string()];
        let a = activate(UpstreamType::NIM, "MistralAI/Mistral-Small", &cfg, None, None, 16000);
        assert!(!a.has_thinking);
    }

    // =======================================================================
    // New tests for #57 — AnthropicApi mechanism
    // =======================================================================

    #[test]
    fn anthropic_upstream_gets_thinking_payload() {
        let cfg = test_config();
        let a = activate(UpstreamType::Anthropic, "claude-opus-4-6", &cfg, None, None, 20000);
        assert!(a.has_thinking);
        assert!(a.chat_template_kwargs.is_none());
        let thinking = a.thinking.expect("Anthropic must get thinking payload");
        assert_eq!(thinking["type"], "enabled");
        assert!(thinking["budget_tokens"].as_u64().unwrap() > 0);
    }

    #[test]
    fn anthropic_thinking_budget_clamped_below_max_tokens() {
        let mut cfg = test_config();
        cfg.thinking_budget_tokens = Some(50000);
        let a = activate(UpstreamType::Anthropic, "claude-opus-4-6", &cfg, None, None, 20000);
        let thinking = a.thinking.unwrap();
        let budget = thinking["budget_tokens"].as_u64().unwrap();
        // Must be strictly less than max_tokens (20000)
        assert!(budget < 20000, "budget {} must be < 20000", budget);
    }

    #[test]
    fn anthropic_thinking_honors_client_budget() {
        let cfg = test_config();
        let client = serde_json::json!({"type": "enabled", "budget_tokens": 8000});
        let a =
            activate(UpstreamType::Anthropic, "claude-opus-4-6", &cfg, None, Some(&client), 20000);
        let thinking = a.thinking.unwrap();
        assert_eq!(thinking["budget_tokens"], 8000);
    }

    #[test]
    fn anthropic_thinking_uses_config_budget() {
        let mut cfg = test_config();
        cfg.thinking_budget_tokens = Some(12000);
        let a = activate(UpstreamType::Anthropic, "claude-opus-4-6", &cfg, None, None, 20000);
        let thinking = a.thinking.unwrap();
        assert_eq!(thinking["budget_tokens"], 12000);
    }

    // =======================================================================
    // New tests for temperature forcing (verified at call site in transform.rs)
    // =======================================================================

    #[test]
    fn openai_openrouter_get_nothing() {
        let cfg = test_config();
        for ut in [UpstreamType::OpenAI, UpstreamType::OpenRouter] {
            let a = activate(ut, "gpt-4o", &cfg, None, None, 16000);
            assert!(!a.has_thinking);
            assert!(a.chat_template_kwargs.is_none());
            assert!(a.thinking.is_none());
        }
    }

    // =======================================================================
    // New tests for per-route override (#58)
    // =======================================================================

    #[test]
    fn route_override_wins_over_default() {
        let cfg = test_config();
        // NIM would default to ChatTemplate, but route says None
        let a = activate(
            UpstreamType::NIM,
            "some-model",
            &cfg,
            Some(ThinkingMechanism::None),
            None,
            16000,
        );
        assert!(!a.has_thinking);
        assert!(a.chat_template_kwargs.is_none());
    }

    #[test]
    fn route_override_wins_over_denylist() {
        let mut cfg = test_config();
        cfg.disable_thinking_models = vec!["mistral".to_string()];
        // Denylist would block, but route says ChatTemplate explicitly
        let a = activate(
            UpstreamType::NIM,
            "mistralai/mistral",
            &cfg,
            Some(ThinkingMechanism::ChatTemplate),
            None,
            16000,
        );
        assert!(a.has_thinking);
        assert!(a.chat_template_kwargs.is_some());
    }

    #[test]
    fn route_override_anthropic_api_on_nim() {
        let cfg = test_config();
        // NIM upstream but route explicitly requests AnthropicApi
        let a = activate(
            UpstreamType::NIM,
            "some-model",
            &cfg,
            Some(ThinkingMechanism::AnthropicApi),
            None,
            20000,
        );
        assert!(a.has_thinking);
        assert!(a.chat_template_kwargs.is_none());
        assert!(a.thinking.is_some());
    }
}
