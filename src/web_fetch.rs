//! WebFetch Interceptor — ejecuta Fetch() localmente
//!
//! Cuando NIM/Qwen3.5 responde con tool_use "web_fetch",
//! This module intercepts, executes local HTTP GET,
//! y devuelve tool_result con el contenido.

use crate::config::Config;
use crate::error::ProxyError;
// use ipnet::IpNet;
use regex::Regex;
use reqwest::Client;
use serde_json::Value;
use std::net::IpAddr;
use std::time::Duration;

/// User-Agent que simula Chrome para evitar bloqueos
const USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) \
    AppleWebKit/537.36 (KHTML, like Gecko) \
    Chrome/131.0.0.0 Safari/537.36";

/// Maximum characters in response (~50k tokens)
const MAX_CONTENT_CHARS: usize = 200_000;

// ========== DETECCIÓN ==========

/// Detecta si un nombre de tool es web_fetch
pub fn is_web_fetch_tool(name: &str) -> bool {
    name == "web_fetch"
        || name.starts_with("web_fetch_") // web_fetch_20260209
        || name == "WebFetch"
}

// ========== EJECUCIÓN ==========

// v0.11.0 (CR-05): SSRF protection — block requests to internal/metadata endpoints
fn is_url_safe(url: &str) -> bool {
    // Extract host from URL
    let host = match url.split("://").nth(1) {
        Some(rest) => rest.split('/').next().unwrap_or("").split(':').next().unwrap_or(""),
        None => return false,
    };

    let blocked_prefixes = [
        "127.",
        "0.0.0.0",
        "localhost",
        "169.254.", // AWS/GCP metadata
        "10.",      // RFC1918 Class A
        "172.16.",
        "172.17.",
        "172.18.",
        "172.19.",
        "172.20.",
        "172.21.",
        "172.22.",
        "172.23.",
        "172.24.",
        "172.25.",
        "172.26.",
        "172.27.",
        "172.28.",
        "172.29.",
        "172.30.",
        "172.31.",  // RFC1918 Class B
        "192.168.", // RFC1918 Class C
        "[::1]",
        "[::0]", // IPv6 loopback/unspecified
    ];

    let blocked_exact = ["metadata.google.internal", "metadata.google"];

    if blocked_prefixes.iter().any(|b| host.starts_with(b)) {
        return false;
    }
    if blocked_exact.iter().any(|b| host.eq_ignore_ascii_case(b)) {
        return false;
    }

    true
}

/// True if `ip` is loopback, unspecified, private (RFC1918/ULA), link-local/CGNAT, or
/// otherwise unsafe to fetch from (SSRF guard, Issue #64). IPv4-mapped IPv6 (e.g.
/// `::ffff:127.0.0.1`) is canonicalized to its IPv4 form first so it cannot bypass the check.
fn is_private_ip(ip: IpAddr) -> bool {
    // Canonicalize IPv4-mapped IPv6 to IPv4 (defeats `::ffff:<private>` bypass).
    let ip = match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(IpAddr::V6(v6)),
        v4 => v4,
    };
    if ip.is_loopback() || ip.is_unspecified() {
        return true;
    }
    const BLOCKED: &[&str] = &[
        "0.0.0.0/8",      // "this network"
        "10.0.0.0/8",     // RFC1918
        "100.64.0.0/10",  // CGNAT (RFC6598)
        "127.0.0.0/8",    // loopback
        "169.254.0.0/16", // link-local / cloud metadata (AWS/GCP/Azure)
        "172.16.0.0/12",  // RFC1918
        "192.168.0.0/16", // RFC1918
        "::1/128",        // IPv6 loopback
        "fc00::/7",       // IPv6 unique local (ULA)
        "fe80::/10",      // IPv6 link-local
    ];
    BLOCKED.iter().any(|cidr| cidr.parse::<ipnet::IpNet>().is_ok_and(|net| net.contains(&ip)))
}

/// DNS-aware SSRF defense (Issue #64): resolve the URL host and reject if any resolved
/// address is private/loopback/link-local. Closes the DNS-rebinding and numeric-encoding
/// bypasses that the string-prefix `is_url_safe()` cannot catch (e.g. a public domain that
/// resolves to `127.0.0.1`, or `127-0-0-1.nip.io`).
///
/// Note: a strict TOCTOU gap remains between resolve and connect; for the proxy's threat
/// model (model-driven fetches) this DNS check plus `is_url_safe()` is a strong mitigation.
async fn verify_host_resolves_public(url: &str) -> Result<(), String> {
    let parsed = reqwest::Url::parse(url).map_err(|e| format!("invalid URL: {e}"))?;
    let host = parsed.host_str().ok_or_else(|| "URL has no host".to_string())?;

    // Host is an IP literal (IPv4 or IPv6): validate directly, no DNS needed.
    if let Ok(ip) = host.parse::<IpAddr>() {
        return if is_private_ip(ip) {
            Err(format!("host is a private/loopback IP ({ip})"))
        } else {
            Ok(())
        };
    }

    // Hostname: resolve and verify EVERY returned address is public.
    // CodeRabbit: bound the DNS lookup so a slow/unresponsive authoritative nameserver
    // (attacker-controlled, since fetches are model-driven) cannot stall the request path.
    let port = parsed.port_or_known_default().unwrap_or(80);
    let addrs: Vec<std::net::SocketAddr> = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        tokio::net::lookup_host((host, port)),
    )
    .await
    .map_err(|_| format!("DNS resolution timed out for '{host}'"))?
    .map_err(|e| format!("DNS resolution failed for '{host}': {e}"))?
    .collect();
    if addrs.is_empty() {
        return Err(format!("'{host}' resolved to no addresses"));
    }
    for addr in &addrs {
        if is_private_ip(addr.ip()) {
            return Err(format!("'{host}' resolves to private/loopback IP {}", addr.ip()));
        }
    }
    Ok(())
}

/// Ejecuta HTTP GET y devuelve contenido como texto
pub async fn execute_fetch(
    client: &Client,
    url: &str,
    config: &Config,
) -> Result<String, ProxyError> {
    tracing::info!("[WebFetch] Intercepted: fetching {}", url);

    // Validar URL
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err(ProxyError::WebFetch(format!(
            "Invalid URL (must start with http/https): {}",
            url
        )));
    }

    // v0.11.0 (CR-05): SSRF protection — fast string pre-filter (internal/metadata hosts).
    if !is_url_safe(url) {
        tracing::warn!("[GUARD] SSRF blocked: {}", url);
        return Err(ProxyError::WebFetch(format!(
            "URL blocked by security policy (internal/metadata address): {}",
            url
        )));
    }

    // Issue #64: DNS-aware SSRF guard — resolve the host and reject private/loopback IPs.
    // Closes DNS rebinding and numeric-encoding bypasses the string filter above misses.
    if let Err(reason) = verify_host_resolves_public(url).await {
        tracing::warn!("[GUARD] SSRF blocked ({}): {}", reason, url);
        return Err(ProxyError::WebFetch(format!(
            "URL blocked by security policy (resolves to internal address): {}",
            url
        )));
    }

    // HTTP GET con timeout y User-Agent
    let response = client
        .get(url)
        .header("User-Agent", USER_AGENT)
        .header("Accept", "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8")
        .header("Accept-Language", "en-US,en;q=0.9,es;q=0.8")
        .timeout(Duration::from_secs(config.web_fetch_timeout_secs))
        .send()
        .await
        .map_err(|e| ProxyError::WebFetch(format!("HTTP request failed for {}: {}", url, e)))?;

    let status = response.status();
    if !status.is_success() {
        return Err(ProxyError::WebFetch(format!("HTTP {} for {}", status, url)));
    }

    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    let body = response
        .text()
        .await
        .map_err(|e| ProxyError::WebFetch(format!("Failed to read body: {}", e)))?;

    // Si es JSON, devolver raw
    if content_type.contains("application/json") {
        let truncated = truncate_content(&body);
        tracing::info!("[WebFetch] JSON response: {} bytes", truncated.len());
        return Ok(truncated);
    }

    // Si es texto plano, devolver raw
    if content_type.contains("text/plain") {
        let truncated = truncate_content(&body);
        tracing::info!("[WebFetch] Plain text: {} bytes", truncated.len());
        return Ok(truncated);
    }

    // Si es HTML, strip tags
    let text = strip_html_tags(&body);
    let truncated = truncate_content(&text);
    tracing::info!("[WebFetch] HTML->text: {} -> {} bytes", body.len(), truncated.len());
    Ok(truncated)
}

// v0.11.0 (HI-02): Cached regex patterns — compiled once, reused on every call
use std::sync::OnceLock;

struct HtmlRegexes {
    script: Regex,
    style: Regex,
    nav: Regex,
    footer: Regex,
    header: Regex,
    headings: [Regex; 6],
    br: Regex,
    block_end: Regex,
    tags: Regex,
    spaces: Regex,
    newlines: Regex,
}

fn html_regexes() -> &'static HtmlRegexes {
    static REGEXES: OnceLock<HtmlRegexes> = OnceLock::new();
    REGEXES.get_or_init(|| HtmlRegexes {
        script: Regex::new(r"(?si)<script[^>]*>.*?</script>").unwrap(),
        style: Regex::new(r"(?si)<style[^>]*>.*?</style>").unwrap(),
        nav: Regex::new(r"(?si)<nav[^>]*>.*?</nav>").unwrap(),
        footer: Regex::new(r"(?si)<footer[^>]*>.*?</footer>").unwrap(),
        header: Regex::new(r"(?si)<header[^>]*>.*?</header>").unwrap(),
        headings: [
            Regex::new(r"(?si)<h1[^>]*>(.*?)</h1>").unwrap(),
            Regex::new(r"(?si)<h2[^>]*>(.*?)</h2>").unwrap(),
            Regex::new(r"(?si)<h3[^>]*>(.*?)</h3>").unwrap(),
            Regex::new(r"(?si)<h4[^>]*>(.*?)</h4>").unwrap(),
            Regex::new(r"(?si)<h5[^>]*>(.*?)</h5>").unwrap(),
            Regex::new(r"(?si)<h6[^>]*>(.*?)</h6>").unwrap(),
        ],
        br: Regex::new(r"(?i)<br\s*/?>").unwrap(),
        block_end: Regex::new(r"(?i)</(?:p|div|li|tr|h[1-6])>").unwrap(),
        tags: Regex::new(r"<[^>]+>").unwrap(),
        spaces: Regex::new(r"[ \t]+").unwrap(),
        newlines: Regex::new(r"\n{3,}").unwrap(),
    })
}

// v0.11.0: Cached regex for URL extraction fallback
fn url_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"https?://[^\s"',\}]+"#).unwrap())
}

/// Elimina tags HTML y extrae texto legible
pub fn strip_html_tags(html: &str) -> String {
    let re = html_regexes();
    let mut text = html.to_string();

    // 1. Eliminar <script>, <style>
    text = re.script.replace_all(&text, "").to_string();
    text = re.style.replace_all(&text, "").to_string();

    // 2. Eliminar <nav>, <footer>, <header>
    text = re.nav.replace_all(&text, "").to_string();
    text = re.footer.replace_all(&text, "").to_string();
    text = re.header.replace_all(&text, "").to_string();

    // 3. Convertir headings a markdown-style
    let hashes = ["#", "##", "###", "####", "#####", "######"];
    for (i, heading_re) in re.headings.iter().enumerate() {
        let replacement = format!("\n{} $1\n", hashes[i]);
        text = heading_re.replace_all(&text, replacement.as_str()).to_string();
    }

    // 4. Convertir <br>, </p>, </div>, </li> a newlines
    text = re.br.replace_all(&text, "\n").to_string();
    text = re.block_end.replace_all(&text, "\n").to_string();

    // 5. Strip remaining HTML tags
    text = re.tags.replace_all(&text, "").to_string();

    // 6. Decode common entities
    text = text
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ");

    // 7. Collapse whitespace
    text = re.spaces.replace_all(&text, " ").to_string();
    text = re.newlines.replace_all(&text, "\n\n").to_string();

    text.trim().to_string()
}

// ========== UTILIDADES ==========

/// Truncates content to maximum allowed
fn truncate_content(text: &str) -> String {
    if text.len() <= MAX_CONTENT_CHARS {
        text.to_string()
    } else {
        let truncated = crate::str_utils::safe_truncate(text, MAX_CONTENT_CHARS);
        format!("{}\n\n[Content truncated at {} characters]", truncated, MAX_CONTENT_CHARS)
    }
}

/// Extrae URL del input JSON de web_fetch
/// Con fallback regex para JSON malformado de Qwen3.5
pub fn extract_url(input: &Value) -> Option<String> {
    // Try standard JSON path first
    if let Some(url) = input.get("url").and_then(|v| v.as_str()) {
        return Some(url.to_string());
    }
    None
}

/// Extrae URL de un string raw (para streaming donde el JSON puede estar malformado)
pub fn extract_url_from_raw(raw: &str) -> Option<String> {
    // Try JSON parse first
    if let Ok(parsed) = serde_json::from_str::<Value>(raw) {
        if let Some(url) = extract_url(&parsed) {
            return Some(url);
        }
    }

    // Regex fallback: find first https?://... URL in the string
    // v0.11.0 (HI-02): Use cached regex
    if let Some(m) = url_regex().find(raw) {
        return Some(m.as_str().to_string());
    }

    None
}

// ========== TESTS ==========

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_web_fetch_tool() {
        assert!(is_web_fetch_tool("web_fetch"));
        assert!(is_web_fetch_tool("web_fetch_20260209"));
        assert!(is_web_fetch_tool("WebFetch"));
        assert!(!is_web_fetch_tool("search"));
        assert!(!is_web_fetch_tool("fetch_markdown"));
    }

    #[test]
    fn test_strip_html_basic() {
        let html = "<html><body><h1>Title</h1><p>Hello world</p></body></html>";
        let text = strip_html_tags(html);
        assert!(text.contains("# Title"));
        assert!(text.contains("Hello world"));
    }

    #[test]
    fn test_strip_html_removes_script() {
        let html = "<p>Before</p><script>alert('xss')</script><p>After</p>";
        let text = strip_html_tags(html);
        assert!(text.contains("Before"));
        assert!(text.contains("After"));
        assert!(!text.contains("alert"));
    }

    #[test]
    fn test_extract_url() {
        let input = serde_json::json!({"url": "https://example.com"});
        assert_eq!(extract_url(&input), Some("https://example.com".into()));

        let empty = serde_json::json!({});
        assert_eq!(extract_url(&empty), None);
    }

    #[test]
    fn test_truncate() {
        let short = "hello";
        assert_eq!(truncate_content(short), "hello");

        let long = "a".repeat(MAX_CONTENT_CHARS + 100);
        let result = truncate_content(&long);
        assert!(result.len() < long.len());
        assert!(result.contains("[Content truncated"));
    }

    // v0.11.0 (CR-05): SSRF protection tests
    #[test]
    fn test_url_safety_blocks_internal() {
        assert!(!is_url_safe("http://127.0.0.1/admin"));
        assert!(!is_url_safe("http://localhost:8080/secret"));
        assert!(!is_url_safe("http://10.0.0.1/internal"));
        assert!(!is_url_safe("http://172.16.0.1/private"));
        assert!(!is_url_safe("http://192.168.1.1/router"));
        assert!(!is_url_safe("http://169.254.169.254/latest/meta-data/"));
        assert!(!is_url_safe("http://0.0.0.0/anything"));
        assert!(!is_url_safe("http://metadata.google.internal/computeMetadata/v1/"));
    }

    #[test]
    fn test_url_safety_allows_external() {
        assert!(is_url_safe("https://example.com/page"));
        assert!(is_url_safe("https://api.github.com/repos"));
        assert!(is_url_safe("https://docs.rs/crate/tokio"));
        assert!(is_url_safe("http://8.8.8.8/dns"));
        assert!(is_url_safe("https://172.217.14.206/search")); // Google public IP, not 172.16-31.*
    }

    // v0.11.0 (HI-02): Regex caching test
    #[test]
    fn test_regex_lazy_init_consistency() {
        // Call strip_html_tags twice — second call should use cached regexes
        let html = "<p>Test <b>bold</b></p>";
        let r1 = strip_html_tags(html);
        let r2 = strip_html_tags(html);
        assert_eq!(r1, r2); // Same result = regex cache is consistent
    }

    // v0.13.0: DNS-based SSRF protection tests
    #[test]
    fn test_private_ip_detection() {
        use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

        // Private IPs should be blocked
        assert!(is_private_ip(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))));
        assert!(is_private_ip(IpAddr::V4(Ipv4Addr::new(172, 16, 0, 1))));
        assert!(is_private_ip(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1))));
        assert!(is_private_ip(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))));
        assert!(is_private_ip(IpAddr::V6(Ipv6Addr::LOCALHOST)));

        // Public IPs should be allowed
        assert!(!is_private_ip(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))));
        assert!(!is_private_ip(IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1))));
    }

    // Issue #64: extended SSRF coverage — encodings, IPv6, IPv4-mapped, CGNAT, link-local.
    #[test]
    fn test_private_ip_extended_vectors() {
        use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
        // Unspecified / "this network"
        assert!(is_private_ip(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0))));
        assert!(is_private_ip(IpAddr::V6(Ipv6Addr::UNSPECIFIED)));
        // Link-local / cloud metadata
        assert!(is_private_ip(IpAddr::V4(Ipv4Addr::new(169, 254, 169, 254))));
        // CGNAT
        assert!(is_private_ip(IpAddr::V4(Ipv4Addr::new(100, 64, 0, 1))));
        // IPv6 ULA + link-local
        assert!(is_private_ip("fc00::1".parse().unwrap()));
        assert!(is_private_ip("fe80::1".parse().unwrap()));
        // IPv4-mapped IPv6 of a loopback address must be caught (canonicalization)
        assert!(is_private_ip("::ffff:127.0.0.1".parse().unwrap()));
        assert!(is_private_ip("::ffff:10.0.0.1".parse().unwrap()));
        // Public IPv4-mapped is still public
        assert!(!is_private_ip("::ffff:8.8.8.8".parse().unwrap()));
        // Public IPv6
        assert!(!is_private_ip("2606:4700:4700::1111".parse().unwrap()));
    }

    #[tokio::test]
    async fn test_verify_host_resolves_public_blocks_internal() {
        // IP literals (no DNS)
        assert!(verify_host_resolves_public("http://127.0.0.1/admin").await.is_err());
        assert!(verify_host_resolves_public("http://169.254.169.254/latest/").await.is_err());
        assert!(verify_host_resolves_public("http://[::1]:8080/").await.is_err());
        assert!(verify_host_resolves_public("http://0.0.0.0/").await.is_err());
        // Hostname that resolves to loopback (reliable via /etc/hosts, no external network)
        assert!(verify_host_resolves_public("http://localhost:8899/poison").await.is_err());
    }

    #[tokio::test]
    async fn test_verify_host_resolves_public_allows_public_ip_literals() {
        // Public IP literals resolve fine without DNS.
        assert!(verify_host_resolves_public("http://8.8.8.8/").await.is_ok());
        assert!(verify_host_resolves_public("https://1.1.1.1/").await.is_ok());
    }
}
