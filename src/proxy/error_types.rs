/// Parsed NIM/OpenAI error response with structured fields
#[derive(Debug, Default)]
pub(crate) struct UpstreamError {
    pub(crate) status: u16,
    pub(crate) message: String,
    pub(crate) error_type: Option<String>, // NIM: "BadRequestError", etc.
    pub(crate) param: Option<String>,      // NIM: "input_tokens", etc.
    /// Error code field — reserved for future detailed error code handling
    /// Tracking: Future integration for error code analysis (PHASE 3.5)
    #[allow(dead_code)]
    pub(crate) code: Option<String>, // NIM: "400", etc.
}

/// Parse NIM/OpenAI error response to extract structured error info.
/// Handles nested errors where NIM wraps a 400 inside a 502.
pub(crate) fn parse_upstream_error(status: u16, body: &str) -> UpstreamError {
    let mut err = UpstreamError { status, message: body.to_string(), ..Default::default() };

    if let Ok(json) = serde_json::from_str::<serde_json::Value>(body) {
        if let Some(error_obj) = json.get("error") {
            err.message =
                error_obj.get("message").and_then(|v| v.as_str()).unwrap_or(body).to_string();
            err.error_type = error_obj.get("type").and_then(|v| v.as_str()).map(String::from);
            err.param = error_obj.get("param").and_then(|v| v.as_str()).map(String::from);
            err.code = error_obj.get("code").and_then(|v| match v {
                serde_json::Value::String(s) => Some(s.clone()),
                serde_json::Value::Number(n) => Some(n.to_string()),
                _ => None,
            });

            // NIM wraps errors: "Upstream returned 400 Bad Request: {...}"
            if let Some(inner) = extract_nested_error(&err.message) {
                tracing::debug!(
                    "🔍 Unwrapped nested error: {} → {}",
                    &err.message.chars().take(80).collect::<String>(),
                    &inner.message.chars().take(80).collect::<String>()
                );
                // Issue #34 Q5: If nested error has a valid status, use it
                // for classify_error(). Inner status is more precise than
                // the wrapper status (e.g., NIM wraps 400 inside 502).
                if inner.status > 0 && inner.status != err.status {
                    tracing::debug!("🔍 Inner status override: {} → {}", err.status, inner.status);
                    err.status = inner.status;
                }
                err.message = inner.message;
                if err.error_type.is_none() {
                    err.error_type = inner.error_type;
                }
                if err.param.is_none() {
                    err.param = inner.param;
                }
            }
        }
    }

    err
}

/// Extract nested error from NIM's wrapper format.
/// NIM sends: "Upstream returned 400 Bad Request: {\"status\":400,\"detail\":\"...\"}"
pub(crate) fn extract_nested_error(msg: &str) -> Option<UpstreamError> {
    let json_start = msg.find('{')?;
    let json_str = &msg[json_start..];
    let json: serde_json::Value = serde_json::from_str(json_str).ok()?;

    Some(UpstreamError {
        status: json.get("status").and_then(|v| v.as_u64()).unwrap_or(0) as u16,
        message: json
            .get("detail")
            .or_else(|| json.get("message"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        error_type: json.get("type").and_then(|v| v.as_str()).map(String::from),
        param: None,
        code: None,
    })
}
