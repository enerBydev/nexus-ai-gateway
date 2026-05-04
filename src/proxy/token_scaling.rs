//! Token scaling between upstream context and Claude Code's context window.
//!
//! When the upstream API has a different context window than Claude Code,
//! raw token counts from the upstream would mislead CC's auto-compact logic.
//! This module scales both input and output tokens proportionally so CC
//! sees consistent usage data that matches its own context window.

/// System prompt overhead that CC reserves (matches CC binary `fi1 = 20000`).
pub const CC_SYSTEM_OVERHEAD_TOKENS: u32 = 20_000;

/// Auto-compact buffer that CC subtracts from effective window
/// (matches CC binary `re8 = 13000`).
#[allow(dead_code)] // Used in tests + doc reference; not called from production binary
pub const CC_AUTOCOMPACT_BUFFER_TOKENS: u32 = 13_000;

/// Default context window for standard Claude models
/// (matches CC binary `vW8 = 200000`).
#[allow(dead_code)]
pub const DEFAULT_CC_CONTEXT_WINDOW: u32 = 200_000;

/// Scaled token pair — both input and output must be scaled consistently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScaledTokens {
    pub input: u32,
    pub output: u32,
}

/// Parameters for token scaling in response transformation.
/// When provided, `openai_to_anthropic()` will scale token counts
/// to match Claude Code's context window.
#[derive(Debug, Clone, Copy)]
pub struct TokenScalingParams {
    pub context_limit: u32,
    pub cc_context_window: u32,
}

/// Scale token usage between upstream context and CC's context window.
///
/// # Branch 1: `context_limit < cc_context_window`
/// Upstream has LESS context than CC. Inflate tokens proportionally so
/// CC sees approaching its limit sooner (prevents overflow on retry).
///
/// # Branch 2: `context_limit >= cc_context_window`
/// Upstream has equal or MORE context than CC. Report real tokens —
/// CC manages its own window and will compact when it approaches its
/// effective threshold (cc_context_window - overhead - buffer).
///
/// # Arguments
/// * `real_input_tokens` — Input tokens reported by upstream
/// * `real_output_tokens` — Output tokens reported by upstream
/// * `context_limit` — Upstream model's context window size
/// * `cc_context_window` — Claude Code's effective context window
/// * `source` — Identifier for logging/debugging (e.g., "streaming", "non-streaming")
///
/// # Returns
/// `ScaledTokens` with both `input` and `output` scaled by the same factor.
pub fn scale_token_usage(
    real_input_tokens: u32,
    real_output_tokens: u32,
    context_limit: u32,
    cc_context_window: u32,
    source: &str,
) -> ScaledTokens {
    if context_limit == 0 || context_limit == cc_context_window {
        // No scaling needed — either unknown context or equal context
        return ScaledTokens { input: real_input_tokens, output: real_output_tokens };
    }

    if context_limit < cc_context_window {
        // Branch 1: Upstream has LESS context than CC
        // Inflate so CC sees approaching its limit proportionally
        let scale_factor = cc_context_window as f64 / context_limit as f64;
        let scaled_input = (real_input_tokens as f64 * scale_factor) as u32;
        let scaled_output = (real_output_tokens as f64 * scale_factor) as u32;
        // Cap at CC's window — never report more than CC can handle
        let capped_input = scaled_input.min(cc_context_window);
        let capped_output = scaled_output.min(cc_context_window);
        tracing::debug!(
            source,
            real_input_tokens,
            real_output_tokens,
            scaled_input = capped_input,
            scaled_output = capped_output,
            scale_factor,
            "Scaling up: upstream_ctx={}K < cc_ctx={}K",
            context_limit / 1000,
            cc_context_window / 1000
        );
        ScaledTokens { input: capped_input, output: capped_output }
    } else {
        // Branch 2: Upstream has equal or MORE context than CC
        // Report real tokens — CC manages its own window
        // CC will compact at: effective_window - buffer
        //   = (cc_context_window - min(max_tokens, 20K)) - 13K
        tracing::debug!(
            source,
            real_input_tokens,
            real_output_tokens,
            "No scaling: upstream_ctx={}K >= cc_ctx={}K, CC manages its own window",
            context_limit / 1000,
            cc_context_window / 1000
        );
        ScaledTokens { input: real_input_tokens, output: real_output_tokens }
    }
}

/// Model-specific max_tokens defaults.
/// Matches Claude Code binary function FHH() (v2.1.87).
///
/// Returns (default, upper_limit) for each model tier.
pub fn resolve_model_max_tokens(model_id: &str) -> (u32, u32) {
    let id = model_id.to_lowercase();

    // FHH() exact mapping from CC binary
    if id.contains("opus-4-6") {
        (64_000, 128_000)
    } else if id.contains("sonnet-4-6") {
        (32_000, 128_000)
    } else if id.contains("opus-4-5") || id.contains("sonnet-4") || id.contains("haiku-4") {
        (32_000, 64_000)
    } else if id.contains("opus-4-1") || id.contains("opus-4") {
        (32_000, 32_000)
    } else if id.contains("claude-3-sonnet") {
        (8_192, 8_192)
    } else if id.contains("claude-3-haiku") {
        (4_096, 4_096)
    } else if id.contains("3-5-sonnet") || id.contains("3-5-haiku") {
        (8_192, 8_192)
    } else if id.contains("3-7-sonnet") {
        (32_000, 64_000)
    } else {
        // Default fallback (matches CC binary DM9 = 64000)
        (32_000, 64_000)
    }
}

/// Calculate CC's effective context window by subtracting system overhead.
/// Matches CC binary function Pd():
///   effective_window = context_window - min(max_tokens, CC_SYSTEM_OVERHEAD_TOKENS)
///
/// This ensures the proxy's overflow threshold aligns with CC's actual
/// auto-compact trigger (effective_window - CC_AUTOCOMPACT_BUFFER_TOKENS).
pub fn resolve_effective_cc_context_window(raw_cc_context_window: u32, model_id: &str) -> u32 {
    let (max_tokens_default, _) = resolve_model_max_tokens(model_id);
    let reserved = max_tokens_default.min(CC_SYSTEM_OVERHEAD_TOKENS);
    raw_cc_context_window.saturating_sub(reserved)
}

#[cfg(test)]
mod tests {
    use super::*;

    // === Branch 1: upstream < CC context ===

    #[test]
    fn test_branch1_scale_up_input_and_output() {
        let result = scale_token_usage(1000, 200, 100_000, 200_000, "test");
        assert_eq!(result.input, 2000);
        assert_eq!(result.output, 400);
    }

    #[test]
    fn test_branch1_capped_at_cc_window() {
        let result = scale_token_usage(60_000, 1000, 50_000, 200_000, "test");
        assert_eq!(result.input, 200_000);
    }

    #[test]
    fn test_branch1_zero_real_tokens() {
        let result = scale_token_usage(0, 0, 100_000, 200_000, "test");
        assert_eq!(result.input, 0);
        assert_eq!(result.output, 0);
    }

    #[test]
    fn test_branch1_output_tokens_scaled_proportionally() {
        let result = scale_token_usage(50000, 4000, 100_000, 200_000, "test");
        assert_eq!(result.output, 8000);
    }

    // === Branch 2: upstream >= CC context ===

    #[test]
    fn test_branch2_no_inflation_equal_context() {
        let result = scale_token_usage(100_000, 4000, 200_000, 200_000, "test");
        assert_eq!(result.input, 100_000);
        assert_eq!(result.output, 4000);
    }

    #[test]
    fn test_branch2_no_inflation_larger_upstream() {
        let result = scale_token_usage(100_000, 4000, 1_000_000, 200_000, "test");
        assert_eq!(result.input, 100_000);
        assert_eq!(result.output, 4000);
    }

    // === Edge cases ===

    #[test]
    fn test_zero_context_limit_passthrough() {
        let result = scale_token_usage(50_000, 2000, 0, 200_000, "test");
        assert_eq!(result.input, 50_000);
        assert_eq!(result.output, 2000);
    }

    #[test]
    fn test_equal_context_no_scaling() {
        let result = scale_token_usage(50_000, 2000, 200_000, 200_000, "test");
        assert_eq!(result.input, 50_000);
        assert_eq!(result.output, 2000);
    }

    #[test]
    fn test_output_capped_at_cc_window() {
        let result = scale_token_usage(1000, 20_000, 10_000, 200_000, "test");
        assert_eq!(result.output, 200_000);
    }

    // === Model max_tokens ===

    #[test]
    fn test_model_max_tokens_opus_4_6() {
        let (default, upper) = resolve_model_max_tokens("claude-opus-4-6");
        assert_eq!(default, 64_000);
        assert_eq!(upper, 128_000);
    }

    #[test]
    fn test_model_max_tokens_sonnet_4_6() {
        let (default, upper) = resolve_model_max_tokens("claude-sonnet-4-6");
        assert_eq!(default, 32_000);
        assert_eq!(upper, 128_000);
    }

    #[test]
    fn test_model_max_tokens_unknown_model() {
        let (default, upper) = resolve_model_max_tokens("some-unknown-model");
        assert_eq!(default, 32_000);
        assert_eq!(upper, 64_000);
    }

    // === Effective context window ===

    #[test]
    fn test_effective_context_window_opus_4_6() {
        let effective = resolve_effective_cc_context_window(200_000, "claude-opus-4-6");
        assert_eq!(effective, 180_000);
    }

    #[test]
    fn test_effective_context_window_sonnet_4_6() {
        let effective = resolve_effective_cc_context_window(200_000, "claude-sonnet-4-6");
        assert_eq!(effective, 180_000);
    }

    #[test]
    fn test_effective_context_window_old_model() {
        let effective = resolve_effective_cc_context_window(200_000, "claude-3-sonnet");
        assert_eq!(effective, 191808);
    }

    #[test]
    fn test_effective_context_window_custom_window() {
        let effective = resolve_effective_cc_context_window(100_000, "claude-opus-4-6");
        assert_eq!(effective, 80_000);
    }

    #[test]
    fn test_compact_trigger_alignment() {
        let effective = resolve_effective_cc_context_window(200_000, "claude-opus-4-6");
        let compact_trigger = effective - CC_AUTOCOMPACT_BUFFER_TOKENS;
        assert_eq!(compact_trigger, 167_000);
        let overflow_90 = effective * 90 / 100;
        assert!(overflow_90 < compact_trigger, "overflow at 90% should fire before CC compact");
    }
}
