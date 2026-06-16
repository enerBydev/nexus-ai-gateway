//! Reasoning activation policy — ARB "Eje A" (Issue #90-B).
//!
//! Replaces the hardcoded `has_thinking = true` + fixed `chat_template_kwargs` with a
//! policy that injects the intent "reason at maximum" translated to the mechanism the
//! upstream accepts. Safety invariant: `chat_template_kwargs` (`enable_thinking`) is
//! emitted ONLY for NIM upstreams — never sent to an upstream that would reject it
//! (Anthropic / OpenAI / OpenRouter handle thinking natively).
//!
//! Policy `global_max` (the registered decision) forces maximum reasoning on every
//! route, decoupling reasoning from the Claude model id — Haiku / Sonnet / deprecated
//! ids no longer cap it (the id now drives only routing + the context window). The
//! default reproduces the previous behavior exactly. The per-model auto-probe
//! (`ReasoningProfile`) that refines the mechanism per NIM model is a documented
//! follow-up; the default activation is safe without it.

use crate::config::UpstreamType;

/// Resolved reasoning activation for one request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Activation {
    /// Whether to route to the reasoning model and request thinking.
    pub has_thinking: bool,
    /// `chat_template_kwargs` to attach (NIM upstreams only), or `None`.
    pub chat_template_kwargs: Option<serde_json::Value>,
}

/// Resolve the activation for `upstream_type` under the `global_max` policy: force
/// maximum reasoning regardless of the Claude model id, translating the intent into the
/// mechanism the upstream accepts (NIM -> `enable_thinking` kwargs; native otherwise).
///
/// `REASONING_ACTIVATION_POLICY` is reserved for future tiering; only `global_max` is
/// implemented today and is the default, so this is behavior-preserving.
pub fn activate(upstream_type: UpstreamType) -> Activation {
    let has_thinking = true;
    // Safety invariant: enable_thinking kwargs go ONLY to NIM upstreams.
    let chat_template_kwargs = if upstream_type == UpstreamType::NIM {
        Some(serde_json::json!({ "enable_thinking": true, "clear_thinking": false }))
    } else {
        None
    };
    Activation { has_thinking, chat_template_kwargs }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nim_gets_enable_thinking_kwargs() {
        let a = activate(UpstreamType::NIM);
        assert!(a.has_thinking);
        let kw = a.chat_template_kwargs.expect("NIM must receive kwargs");
        assert_eq!(kw["enable_thinking"], true);
        assert_eq!(kw["clear_thinking"], false);
    }

    #[test]
    fn never_sends_kwargs_to_non_nim() {
        // Safety invariant: Anthropic/OpenAI/OpenRouter must never receive
        // chat_template_kwargs (they would reject the field).
        for ut in [UpstreamType::Anthropic, UpstreamType::OpenAI, UpstreamType::OpenRouter] {
            let a = activate(ut);
            assert!(a.has_thinking);
            assert_eq!(a.chat_template_kwargs, None);
        }
    }

    #[test]
    fn reasoning_independent_of_claude_model_id() {
        // global_max forces reasoning regardless of route; `activate` depends only on the
        // upstream type, never on the Claude model id.
        assert!(activate(UpstreamType::NIM).has_thinking);
        assert!(activate(UpstreamType::Anthropic).has_thinking);
    }
}
