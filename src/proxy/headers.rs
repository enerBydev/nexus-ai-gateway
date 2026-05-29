//! Client header extraction and resolution for Anthropic upstreams.
//!
//! Handles `anthropic-beta` and `anthropic-version` headers sent by Claude Code,
//! merging proxy-required betas (e.g. prompt-caching-scope) with client-provided
//! ones, and providing sensible defaults when the client omits headers.
//!
//! TODO: Remove `allow(dead_code)` once F3 integrates ClientHeaders into the proxy handler.

#![allow(dead_code)]

use axum::http::HeaderMap;

/// Beta IDs que NEXUS garantiza enviar a upstreams Anthropic.
/// Racional: prompt-caching-scope es necesario para cache isolation por workspace.
const PROXY_MINIMUM_BETAS: &[&str] = &["prompt-caching-scope-2026-01-05"];

/// Default `anthropic-version` when the client does not provide one.
const DEFAULT_ANTHROPIC_VERSION: &str = "2023-06-01";

/// Headers relevantes del cliente CC que deben forwardearse a upstreams Anthropic.
#[derive(Debug, Clone)]
pub(crate) struct ClientHeaders {
    /// Valor completo del header `anthropic-beta` del cliente (comma-separated).
    /// `None` si el cliente no envió el header.
    pub anthropic_beta: Option<String>,

    /// Valor del header `anthropic-version` del cliente.
    /// `None` si el cliente no envió el header.
    pub anthropic_version: Option<String>,
}

impl ClientHeaders {
    /// Extract `anthropic-beta` and `anthropic-version` from request headers.
    pub(crate) fn from_headers(headers: &HeaderMap) -> Self {
        let anthropic_beta =
            headers.get("anthropic-beta").and_then(|v| v.to_str().ok()).map(|s| s.to_string());

        let anthropic_version =
            headers.get("anthropic-version").and_then(|v| v.to_str().ok()).map(|s| s.to_string());

        Self { anthropic_beta, anthropic_version }
    }

    /// Resolve the `anthropic-beta` header value to forward upstream.
    ///
    /// Logic (User Decision Q1, Option C):
    /// 1. Client sends betas → merge with `PROXY_MINIMUM_BETAS`, deduplicate
    /// 2. Client does NOT send betas (None or empty) → use only `PROXY_MINIMUM_BETAS`
    /// 3. Result: comma-separated string ready for the header
    pub(crate) fn resolve_anthropic_beta(&self) -> String {
        let mut betas: Vec<&str> = PROXY_MINIMUM_BETAS.to_vec();

        if let Some(ref client_beta) = self.anthropic_beta {
            if !client_beta.is_empty() {
                for beta in client_beta.split(',') {
                    let trimmed = beta.trim();
                    if !trimmed.is_empty() && !betas.contains(&trimmed) {
                        betas.push(trimmed);
                    }
                }
            }
        }

        betas.join(",")
    }

    /// Resolve the `anthropic-version` header value to forward upstream.
    ///
    /// Logic (User Decision Q5, Option A):
    /// 1. Client sends `anthropic-version` → forward it
    /// 2. If not → use default `2023-06-01`
    pub(crate) fn resolve_anthropic_version(&self) -> &str {
        self.anthropic_version.as_deref().unwrap_or(DEFAULT_ANTHROPIC_VERSION)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderName, HeaderValue};

    fn header_map_with(headers: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in headers {
            let hname = HeaderName::from_bytes(name.as_bytes()).expect("valid header name");
            let hvalue = HeaderValue::from_str(value).expect("valid header value");
            map.insert(hname, hvalue);
        }
        map
    }

    #[test]
    fn test_from_headers_extracts_beta() {
        let headers = header_map_with(&[("anthropic-beta", "interleaved-thinking-2025-05-14")]);
        let ch = ClientHeaders::from_headers(&headers);
        assert_eq!(ch.anthropic_beta.as_deref(), Some("interleaved-thinking-2025-05-14"));
    }

    #[test]
    fn test_from_headers_no_beta() {
        let headers = HeaderMap::new();
        let ch = ClientHeaders::from_headers(&headers);
        assert!(ch.anthropic_beta.is_none());
    }

    #[test]
    fn test_from_headers_extracts_version() {
        let headers = header_map_with(&[("anthropic-version", "2024-10-22")]);
        let ch = ClientHeaders::from_headers(&headers);
        assert_eq!(ch.anthropic_version.as_deref(), Some("2024-10-22"));
    }

    #[test]
    fn test_resolve_beta_merges_client_with_proxy_minimum() {
        let ch = ClientHeaders {
            anthropic_beta: Some("interleaved-thinking-2025-05-14,compact-2026-01-12".into()),
            anthropic_version: None,
        };
        let result = ch.resolve_anthropic_beta();
        assert_eq!(
            result,
            "prompt-caching-scope-2026-01-05,interleaved-thinking-2025-05-14,compact-2026-01-12"
        );
    }

    #[test]
    fn test_resolve_beta_deduplicates_overlaps() {
        let ch = ClientHeaders {
            anthropic_beta: Some("prompt-caching-scope-2026-01-05,compact-2026-01-12".into()),
            anthropic_version: None,
        };
        let result = ch.resolve_anthropic_beta();
        assert_eq!(result, "prompt-caching-scope-2026-01-05,compact-2026-01-12");
    }

    #[test]
    fn test_resolve_beta_default_when_none() {
        let ch = ClientHeaders { anthropic_beta: None, anthropic_version: None };
        let result = ch.resolve_anthropic_beta();
        assert_eq!(result, "prompt-caching-scope-2026-01-05");
    }

    #[test]
    fn test_resolve_beta_default_when_empty() {
        let ch = ClientHeaders { anthropic_beta: Some(String::new()), anthropic_version: None };
        let result = ch.resolve_anthropic_beta();
        assert_eq!(result, "prompt-caching-scope-2026-01-05");
    }

    #[test]
    fn test_resolve_version_forwarded() {
        let ch =
            ClientHeaders { anthropic_beta: None, anthropic_version: Some("2024-10-22".into()) };
        assert_eq!(ch.resolve_anthropic_version(), "2024-10-22");
    }

    #[test]
    fn test_resolve_version_default_when_none() {
        let ch = ClientHeaders { anthropic_beta: None, anthropic_version: None };
        assert_eq!(ch.resolve_anthropic_version(), "2023-06-01");
    }

    // =========================================================================
    // Issue #35 F10: Edge case tests for header forwarding
    // =========================================================================

    #[test]
    fn test_beta_header_with_spaces() {
        // Beta values may have spaces around commas
        let ch = ClientHeaders {
            anthropic_beta: Some(" interleaved-thinking-2025-05-14 , compact-2026-01-12 ".into()),
            anthropic_version: None,
        };
        let result = ch.resolve_anthropic_beta();
        // Should trim each beta
        assert!(result.contains("interleaved-thinking-2025-05-14"));
        assert!(result.contains("compact-2026-01-12"));
        assert!(result.contains("prompt-caching-scope-2026-01-05"));
    }

    #[test]
    fn test_multiple_betas_deduplication() {
        // Client sends the same beta multiple times
        let ch = ClientHeaders {
            anthropic_beta: Some(
                "prompt-caching-scope-2026-01-05,prompt-caching-scope-2026-01-05".into(),
            ),
            anthropic_version: None,
        };
        let result = ch.resolve_anthropic_beta();
        // Count occurrences of prompt-caching-scope
        let count = result.matches("prompt-caching-scope-2026-01-05").count();
        assert_eq!(count, 1, "Should deduplicate repeated betas, got: {}", result);
    }

    #[test]
    fn test_from_headers_missing_both() {
        let headers = HeaderMap::new();
        let ch = ClientHeaders::from_headers(&headers);
        assert!(ch.anthropic_beta.is_none());
        assert!(ch.anthropic_version.is_none());
    }

    #[test]
    fn test_from_headers_with_other_headers() {
        // Other headers (like authorization) should be ignored
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::HeaderName::from_static("authorization"),
            axum::http::HeaderValue::from_static("Bearer sk-test"),
        );
        headers.insert(
            axum::http::HeaderName::from_static("anthropic-beta"),
            axum::http::HeaderValue::from_static("test-beta"),
        );
        let ch = ClientHeaders::from_headers(&headers);
        assert_eq!(ch.anthropic_beta.as_deref(), Some("test-beta"));
        assert!(ch.anthropic_version.is_none());
    }

    #[test]
    fn test_version_header_missing_uses_default() {
        let ch = ClientHeaders { anthropic_beta: None, anthropic_version: None };
        assert_eq!(ch.resolve_anthropic_version(), "2023-06-01");
    }
}
