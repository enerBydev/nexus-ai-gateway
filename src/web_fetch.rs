//! WebFetch Interceptor — ejecuta Fetch() localmente
//!
//! Cuando NIM/Qwen3.5 responde con tool_use "web_fetch",
//! This module intercepts, executes local HTTP GET,
//! y devuelve tool_result con el contenido.

use crate::config::Config;
use crate::error::ProxyError;
use regex::Regex;
use reqwest::Client;
use serde_json::Value;
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

    // HTTP GET con timeout y User-Agent
    let response = client
        .get(url)
        .header("User-Agent", USER_AGENT)
        .header(
            "Accept",
            "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
        )
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
        tracing::info!("[WebFetch] JSON response: {} chars", truncated.len());
        return Ok(truncated);
    }

    // Si es texto plano, devolver raw
    if content_type.contains("text/plain") {
        let truncated = truncate_content(&body);
        tracing::info!("[WebFetch] Plain text: {} chars", truncated.len());
        return Ok(truncated);
    }

    // Si es HTML, strip tags
    let text = strip_html_tags(&body);
    let truncated = truncate_content(&text);
    tracing::info!(
        "[WebFetch] HTML→text: {} → {} chars",
        body.len(),
        truncated.len()
    );
    Ok(truncated)
}

// ========== HTML → TEXTO ==========

/// Elimina tags HTML y extrae texto legible
pub fn strip_html_tags(html: &str) -> String {
    let mut text = html.to_string();

    // 1. Eliminar <script>...</script> y <style>...</style>
    let re_script = Regex::new(r"(?si)<script[^>]*>.*?</script>").unwrap();
    text = re_script.replace_all(&text, "").to_string();

    let re_style = Regex::new(r"(?si)<style[^>]*>.*?</style>").unwrap();
    text = re_style.replace_all(&text, "").to_string();

    // 2. Eliminar <nav>, <footer>, <header> (noise)
    let re_nav = Regex::new(r"(?si)<nav[^>]*>.*?</nav>").unwrap();
    text = re_nav.replace_all(&text, "").to_string();

    let re_footer = Regex::new(r"(?si)<footer[^>]*>.*?</footer>").unwrap();
    text = re_footer.replace_all(&text, "").to_string();

    let re_header = Regex::new(r"(?si)<header[^>]*>.*?</header>").unwrap();
    text = re_header.replace_all(&text, "").to_string();

    // 3. Convertir headings a markdown-style BEFORE stripping closing tags
    for level in 1..=6usize {
        let hashes = "#".repeat(level);
        let pattern = format!(r"(?si)<h{}[^>]*>(.*?)</h{}>", level, level);
        let re = Regex::new(&pattern).unwrap();
        let replacement = format!("\n{} $1\n", hashes);
        text = re.replace_all(&text, replacement.as_str()).to_string();
    }

    // 4. Convertir <br>, </p>, </div>, </li> a newlines
    let re_br = Regex::new(r"(?i)<br\s*/?>").unwrap();
    text = re_br.replace_all(&text, "\n").to_string();

    let re_block_end = Regex::new(r"(?i)</(?:p|div|li|tr|h[1-6])>").unwrap();
    text = re_block_end.replace_all(&text, "\n").to_string();

    // 5. Strip remaining HTML tags
    let re_tags = Regex::new(r"<[^>]+>").unwrap();
    text = re_tags.replace_all(&text, "").to_string();

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
    let re_spaces = Regex::new(r"[ \t]+").unwrap();
    text = re_spaces.replace_all(&text, " ").to_string();

    let re_newlines = Regex::new(r"\n{3,}").unwrap();
    text = re_newlines.replace_all(&text, "\n\n").to_string();

    text.trim().to_string()
}

// ========== UTILIDADES ==========

/// Truncates content to maximum allowed
fn truncate_content(text: &str) -> String {
    if text.len() <= MAX_CONTENT_CHARS {
        text.to_string()
    } else {
        let truncated = &text[..MAX_CONTENT_CHARS];
        format!(
            "{}\n\n[Content truncated at {} characters]",
            truncated, MAX_CONTENT_CHARS
        )
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
    let re = Regex::new(r#"https?://[^\s"',\}]+"#).unwrap();
    if let Some(m) = re.find(raw) {
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
}
