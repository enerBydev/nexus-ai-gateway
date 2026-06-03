//! Client fingerprinting — HMAC-SHA256 with instance-specific secret.
//!
//! Security model:
//! - HMAC-SHA256(secret, data) for all sensitive fields
//! - Secret is instance-specific (random 32 bytes, generated on first boot)
//! - Even with source code + DB access, fingerprints are:
//!   - Irreversible (HMAC is one-way)
//!   - Not verifiable without the secret
//!   - Not correlatable across instances (different secrets)

use axum::http::HeaderMap;
use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Client type classification based on User-Agent and request patterns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
#[allow(dead_code)] // AnotherProxy is a placeholder for future pattern analysis
pub enum ClientType {
    ClaudeCode,
    AnthropicSDK,
    OpenAISDK,
    Cline,
    Aider,
    Continue,
    Codex,
    Cursor,
    Windsurf,
    Copilot,
    AnotherProxy,
    CustomScript,
    Unknown,
}

impl std::fmt::Display for ClientType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientType::ClaudeCode => write!(f, "claude_code"),
            ClientType::AnthropicSDK => write!(f, "sdk"),
            ClientType::OpenAISDK => write!(f, "openai_sdk"),
            ClientType::Cline => write!(f, "cline"),
            ClientType::Aider => write!(f, "aider"),
            ClientType::Continue => write!(f, "continue"),
            ClientType::Codex => write!(f, "codex"),
            ClientType::Cursor => write!(f, "cursor"),
            ClientType::Windsurf => write!(f, "windsurf"),
            ClientType::Copilot => write!(f, "copilot"),
            ClientType::AnotherProxy => write!(f, "another_proxy"),
            ClientType::CustomScript => write!(f, "script"),
            ClientType::Unknown => write!(f, "unknown"),
        }
    }
}

/// Fingerprint of a single client request.
/// All identifying fields are HMAC'd — no plaintext PII stored.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ClientFingerprint {
    /// HMAC-SHA256(secret, "ip:" + ip_/24_prefix)
    pub fingerprint_ip: String,
    /// HMAC-SHA256(secret, "key:" + api_key_prefix_8)
    pub fingerprint_key: String,
    /// Classified client type
    pub client_type: ClientType,
    /// Short label for Prometheus
    pub user_agent_category: String,
    /// Number of messages in the request
    pub message_count: u32,
    /// Whether any message contains tool_use content block
    pub has_tool_use: bool,
    /// Whether the request includes a system prompt
    pub has_system_prompt: bool,
    /// Model requested
    pub model: String,
    /// Streaming or not
    pub is_streaming: bool,
}

/// Encode bytes as lowercase hex string (inline to avoid adding `hex` crate).
fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

/// Compute HMAC-SHA256(secret, data) and return as hex string.
fn hmac_sha256(secret: &[u8], data: &str) -> String {
    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC can accept any key length");
    mac.update(data.as_bytes());
    to_hex(&mac.finalize().into_bytes())
}

/// Classify client type from request headers.
///
/// Primary signal: User-Agent header.
/// Secondary signal: anthropic-beta header (present = likely Claude Code).
pub fn classify_client_type(headers: &HeaderMap) -> ClientType {
    let ua = headers.get("user-agent").and_then(|v| v.to_str().ok()).unwrap_or("");
    let ua_lower = ua.to_lowercase();

    // Priority 1: Cline (wraps Anthropic SDK, so check BEFORE anthropic)
    if ua_lower.contains("cline") {
        return ClientType::Cline;
    }

    // Priority 2: Aider (wraps Anthropic SDK, so check BEFORE anthropic)
    if ua_lower.contains("aider") {
        return ClientType::Aider;
    }

    // Priority 3: Continue (AI coding assistant)
    if ua_lower.contains("continue") || ua_lower.contains("continuedev") {
        return ClientType::Continue;
    }

    // Priority 4: Claude Code CLI
    if ua_lower.contains("claude-code") || ua_lower.contains("claudecode") {
        return ClientType::ClaudeCode;
    }

    // Priority 5: OpenAI SDK (before generic anthropic check)
    if ua_lower.contains("openai-python") || ua_lower.contains("openai-node") {
        return ClientType::OpenAISDK;
    }

    // Priority 6: Anthropic SDK
    if ua_lower.contains("anthropic") {
        return ClientType::AnthropicSDK;
    }

    // Priority 7: Codex (OpenAI CLI tool — check via originator header or UA)
    if ua_lower.contains("codex-cli")
        || headers
            .get("x-originator")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_lowercase().contains("codex"))
            .unwrap_or(false)
    {
        return ClientType::Codex;
    }

    // Priority 8: Closed-source tools (best-effort from UA patterns)
    if ua_lower.contains("cursor") {
        return ClientType::Cursor;
    }
    if ua_lower.contains("windsurf") || ua_lower.contains("codeium") {
        return ClientType::Windsurf;
    }
    if ua_lower.contains("copilot") || ua_lower.contains("github-copilot") {
        return ClientType::Copilot;
    }

    // Priority 9: Known script/tool User-Agents
    let script_signatures = [
        "curl/",
        "python-requests",
        "python/",
        "axios/",
        "go-resty",
        "node-fetch",
        "ruby",
        "java/",
        "okhttp",
        "wget",
        "httpie",
        "reqwest",
    ];
    for sig in &script_signatures {
        if ua_lower.contains(sig) {
            return ClientType::CustomScript;
        }
    }

    // Priority 10: anthropic-beta header suggests Claude Code
    if headers.get("anthropic-beta").is_some() {
        return ClientType::ClaudeCode;
    }

    ClientType::Unknown
}

/// Extract IP prefix for fingerprinting.
/// - IPv4: first 3 octets (e.g., "192.168.1.5" → "192.168.1")
/// - IPv6: first segment (simplified — first 4 hex groups)
/// - Loopback: "loopback"
/// - Unspecified: "unknown"
pub fn extract_ip_prefix(addr: &std::net::SocketAddr) -> String {
    let ip = addr.ip();

    if ip.is_loopback() {
        return "loopback".to_string();
    }

    match ip {
        std::net::IpAddr::V4(v4) => {
            let octets = v4.octets();
            format!("{}.{}.{}", octets[0], octets[1], octets[2])
        }
        std::net::IpAddr::V6(v6) => {
            // Use first 64 bits (4 groups) as the prefix
            let segments = v6.segments();
            format!("{:x}:{:x}:{:x}:{:x}", segments[0], segments[1], segments[2], segments[3])
        }
    }
}

/// Compute HMAC fingerprint for an IP address.
/// Returns HMAC-SHA256(secret, "ip:" + ip_prefix).
pub fn fingerprint_ip(addr: &std::net::SocketAddr, secret: &[u8]) -> String {
    let prefix = extract_ip_prefix(addr);
    hmac_sha256(secret, &format!("ip:{prefix}"))
}

/// Extract API key prefix for fingerprinting.
/// Looks for x-api-key header first, then Authorization Bearer.
/// Takes only the first 8 characters.
fn extract_api_key_prefix(headers: &HeaderMap) -> String {
    // Try x-api-key header first (Anthropic convention)
    if let Some(key) =
        headers.get("x-api-key").and_then(|v| v.to_str().ok()).filter(|k| !k.is_empty())
    {
        return key.chars().take(8).collect();
    }

    // Try Authorization Bearer
    if let Some(auth) = headers.get("authorization").and_then(|v| v.to_str().ok()) {
        let key = auth.strip_prefix("Bearer ").unwrap_or(auth);
        if !key.is_empty() {
            return key.chars().take(8).collect();
        }
    }

    "none".to_string()
}

/// Compute HMAC fingerprint for an API key.
/// Returns HMAC-SHA256(secret, "key:" + api_key_prefix_8).
pub fn fingerprint_api_key(headers: &HeaderMap, secret: &[u8]) -> String {
    let prefix = extract_api_key_prefix(headers);
    hmac_sha256(secret, &format!("key:{prefix}"))
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

    // === ClientType classification ===

    #[test]
    fn classify_cline_user_agent() {
        let headers = header_map_with(&[("user-agent", "cline/1.3.5")]);
        assert_eq!(classify_client_type(&headers), ClientType::Cline);
    }

    #[test]
    fn classify_aider_user_agent() {
        let headers = header_map_with(&[("user-agent", "aider/0.45.0")]);
        assert_eq!(classify_client_type(&headers), ClientType::Aider);
    }

    #[test]
    fn classify_aider_before_anthropic() {
        // Aider wraps the Anthropic SDK — must be detected as Aider, not AnthropicSDK
        let headers = header_map_with(&[("user-agent", "aider/0.45.0 anthropic-sdk/python/0.5")]);
        assert_eq!(classify_client_type(&headers), ClientType::Aider);
    }

    #[test]
    fn classify_continue_user_agent() {
        let headers = header_map_with(&[("user-agent", "continuedev/0.8.0")]);
        assert_eq!(classify_client_type(&headers), ClientType::Continue);
    }

    #[test]
    fn classify_openai_sdk_python() {
        let headers = header_map_with(&[("user-agent", "openai-python/1.30.0")]);
        assert_eq!(classify_client_type(&headers), ClientType::OpenAISDK);
    }

    #[test]
    fn classify_codex_user_agent() {
        let headers = header_map_with(&[("user-agent", "codex-cli/1.0.0")]);
        assert_eq!(classify_client_type(&headers), ClientType::Codex);
    }

    #[test]
    fn classify_codex_via_originator_header() {
        let headers = header_map_with(&[("user-agent", "SomeApp/1.0"), ("x-originator", "codex")]);
        assert_eq!(classify_client_type(&headers), ClientType::Codex);
    }

    #[test]
    fn classify_cursor_user_agent() {
        let headers = header_map_with(&[("user-agent", "cursor/0.45.0")]);
        assert_eq!(classify_client_type(&headers), ClientType::Cursor);
    }

    #[test]
    fn classify_windsurf_user_agent() {
        let headers = header_map_with(&[("user-agent", "windsurf/1.0.0")]);
        assert_eq!(classify_client_type(&headers), ClientType::Windsurf);
    }

    #[test]
    fn classify_codeium_user_agent() {
        let headers = header_map_with(&[("user-agent", "codeium/1.2.0")]);
        assert_eq!(classify_client_type(&headers), ClientType::Windsurf);
    }

    #[test]
    fn classify_copilot_user_agent() {
        let headers = header_map_with(&[("user-agent", "github-copilot/1.0.0")]);
        assert_eq!(classify_client_type(&headers), ClientType::Copilot);
    }

    #[test]
    fn classify_cline_before_anthropic() {
        // Cline wraps the Anthropic SDK — must be detected as Cline, not AnthropicSDK
        let headers = header_map_with(&[("user-agent", "cline/1.3.5 anthropic-sdk/python/0.5")]);
        assert_eq!(classify_client_type(&headers), ClientType::Cline);
    }

    // === ClientType classification (original tests) ===

    #[test]
    fn classify_claude_code_user_agent() {
        let headers = header_map_with(&[("user-agent", "claude-code/1.0.3")]);
        assert_eq!(classify_client_type(&headers), ClientType::ClaudeCode);
    }

    #[test]
    fn classify_claudecode_no_hyphen() {
        let headers = header_map_with(&[("user-agent", "ClaudeCode/2.0")]);
        assert_eq!(classify_client_type(&headers), ClientType::ClaudeCode);
    }

    #[test]
    fn classify_anthropic_sdk() {
        let headers = header_map_with(&[("user-agent", "anthropic-sdk/python/0.5")]);
        assert_eq!(classify_client_type(&headers), ClientType::AnthropicSDK);
    }

    #[test]
    fn classify_curl() {
        let headers = header_map_with(&[("user-agent", "curl/8.1.2")]);
        assert_eq!(classify_client_type(&headers), ClientType::CustomScript);
    }

    #[test]
    fn classify_python_requests() {
        let headers = header_map_with(&[("user-agent", "python-requests/2.31")]);
        assert_eq!(classify_client_type(&headers), ClientType::CustomScript);
    }

    #[test]
    fn classify_axios() {
        let headers = header_map_with(&[("user-agent", "axios/1.6.0")]);
        assert_eq!(classify_client_type(&headers), ClientType::CustomScript);
    }

    #[test]
    fn classify_unknown_no_ua() {
        let headers = HeaderMap::new();
        assert_eq!(classify_client_type(&headers), ClientType::Unknown);
    }

    #[test]
    fn classify_unknown_unrecognized_ua() {
        let headers = header_map_with(&[("user-agent", "MyCustomApp/1.0")]);
        assert_eq!(classify_client_type(&headers), ClientType::Unknown);
    }

    #[test]
    fn classify_claude_code_via_beta_header() {
        // No User-Agent with claude-code, but anthropic-beta present
        let headers = header_map_with(&[
            ("user-agent", "SomeApp/1.0"),
            ("anthropic-beta", "interleaved-thinking-2025-05-14"),
        ]);
        assert_eq!(classify_client_type(&headers), ClientType::ClaudeCode);
    }

    // === IP prefix extraction ===

    #[test]
    fn ip_prefix_ipv4() {
        let addr: std::net::SocketAddr = "192.168.1.5:8315".parse().unwrap();
        assert_eq!(extract_ip_prefix(&addr), "192.168.1");
    }

    #[test]
    fn ip_prefix_loopback() {
        let addr: std::net::SocketAddr = "127.0.0.1:8315".parse().unwrap();
        assert_eq!(extract_ip_prefix(&addr), "loopback");
    }

    #[test]
    fn ip_prefix_ipv6_loopback() {
        let addr: std::net::SocketAddr = "[::1]:8315".parse().unwrap();
        assert_eq!(extract_ip_prefix(&addr), "loopback");
    }

    #[test]
    fn ip_prefix_ipv6() {
        let addr: std::net::SocketAddr = "[2001:db8:85a3::8a2e:370:7334]:8315".parse().unwrap();
        let prefix = extract_ip_prefix(&addr);
        assert!(
            prefix.starts_with("2001:db8:85a3"),
            "IPv6 prefix should start with first 4 groups, got: {prefix}"
        );
    }

    // === HMAC fingerprinting ===

    #[test]
    fn hmac_consistency() {
        let secret = b"test-secret-32-bytes-long-enough!!";
        let addr: std::net::SocketAddr = "10.0.1.5:8315".parse().unwrap();
        let fp1 = fingerprint_ip(&addr, secret);
        let fp2 = fingerprint_ip(&addr, secret);
        assert_eq!(fp1, fp2, "Same input + same secret must produce same output");
        assert_eq!(fp1.len(), 64, "HMAC-SHA256 output must be 64 hex chars");
    }

    #[test]
    fn hmac_uniqueness_different_ip() {
        let secret = b"test-secret-32-bytes-long-enough!!";
        let addr1: std::net::SocketAddr = "10.0.1.5:8315".parse().unwrap();
        let addr2: std::net::SocketAddr = "10.0.2.5:8315".parse().unwrap();
        let fp1 = fingerprint_ip(&addr1, secret);
        let fp2 = fingerprint_ip(&addr2, secret);
        assert_ne!(fp1, fp2, "Different inputs must produce different outputs");
    }

    #[test]
    fn hmac_uniqueness_different_secret() {
        let secret1 = b"secret-one-32-bytes-long-enough!!!!";
        let secret2 = b"secret-two-32-bytes-long-enough!!!!";
        let addr: std::net::SocketAddr = "10.0.1.5:8315".parse().unwrap();
        let fp1 = fingerprint_ip(&addr, secret1);
        let fp2 = fingerprint_ip(&addr, secret2);
        assert_ne!(fp1, fp2, "Same input + different secret must produce different outputs");
    }

    #[test]
    fn hmac_same_ip_same_prefix() {
        let secret = b"test-secret-32-bytes-long-enough!!";
        let addr1: std::net::SocketAddr = "10.0.1.5:8315".parse().unwrap();
        let addr2: std::net::SocketAddr = "10.0.1.99:8315".parse().unwrap();
        let fp1 = fingerprint_ip(&addr1, secret);
        let fp2 = fingerprint_ip(&addr2, secret);
        // Same /24 prefix → same fingerprint
        assert_eq!(fp1, fp2, "IPs in same /24 prefix must produce same fingerprint");
    }

    // === API key fingerprinting ===

    #[test]
    fn api_key_x_api_key_header() {
        let secret = b"test-secret-32-bytes-long-enough!!";
        let headers = header_map_with(&[("x-api-key", "sk-ant-api03-abcdefghij")]);
        let fp = fingerprint_api_key(&headers, secret);
        assert_eq!(fp.len(), 64, "HMAC output must be 64 hex chars");
    }

    #[test]
    fn api_key_authorization_bearer() {
        let secret = b"test-secret-32-bytes-long-enough!!";
        let headers = header_map_with(&[("authorization", "Bearer sk-ant-api03-abcdefghij")]);
        let fp = fingerprint_api_key(&headers, secret);
        assert_eq!(fp.len(), 64);
    }

    #[test]
    fn api_key_none_when_missing() {
        let secret = b"test-secret-32-bytes-long-enough!!";
        let headers = HeaderMap::new();
        let fp = fingerprint_api_key(&headers, secret);
        assert_eq!(fp.len(), 64); // HMAC("key:none") — still valid hash
    }

    #[test]
    fn api_key_same_prefix_same_fingerprint() {
        let secret = b"test-secret-32-bytes-long-enough!!";
        let headers1 = header_map_with(&[("x-api-key", "sk-ant-a3-first-key-here")]);
        let headers2 = header_map_with(&[("x-api-key", "sk-ant-a3-second-key-here")]);
        let fp1 = fingerprint_api_key(&headers1, secret);
        let fp2 = fingerprint_api_key(&headers2, secret);
        // Both keys start with "sk-ant-a" (8 chars) → same fingerprint
        assert_eq!(fp1, fp2, "API keys with same 8-char prefix must produce same fingerprint");
    }

    #[test]
    fn api_key_different_prefix_different_fingerprint() {
        let secret = b"test-secret-32-bytes-long-enough!!";
        let headers1 = header_map_with(&[("x-api-key", "sk-ant-a3-first-key-here")]);
        let headers2 = header_map_with(&[("x-api-key", "sk-proj-b7-second-key-here")]);
        let fp1 = fingerprint_api_key(&headers1, secret);
        let fp2 = fingerprint_api_key(&headers2, secret);
        assert_ne!(fp1, fp2, "Different API key prefixes must produce different fingerprints");
    }

    // === to_hex utility ===

    #[test]
    fn to_hex_empty() {
        assert_eq!(to_hex(&[]), "");
    }

    #[test]
    fn to_hex_known_value() {
        // SHA256 of empty string starts with e3b0c44298fc1c14...
        let bytes: [u8; 4] = [0xe3, 0xb0, 0xc4, 0x42];
        assert_eq!(to_hex(&bytes), "e3b0c442");
    }
}
