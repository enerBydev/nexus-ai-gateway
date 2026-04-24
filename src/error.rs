use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;
use thiserror::Error;

/// Application-specific errors
#[derive(Error, Debug)]
pub enum ProxyError {
    /// Configuration error — reserved for future config validation
    /// Tracking: Future integration for config validation (PHASE 3.5)
    #[allow(dead_code)]
    #[error("Configuration error: {0}")]
    Config(String),

    #[error("Request transformation error: {0}")]
    Transform(String),

    #[error("Upstream API error: {0}")]
    Upstream(String),

    #[error("Service overloaded: {0}")]
    Overloaded(String),

    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    /// Internal error — reserved for unexpected internal failures
    /// Tracking: Future integration for internal error handling (PHASE 3.5)
    #[allow(dead_code)]
    #[error("Internal error: {0}")]
    Internal(String),

    #[error("WebFetch error: {0}")]
    WebFetch(String),

    /// v6.1: Context overflow — returns 400 so CC does NOT retry
    #[error("Context overflow: {0}")]
    ContextOverflow(String),

    /// v0.11.0: Stream interrupted — NIM stopped sending chunks
    /// Tracking: Future integration for stream timeout handling (PHASE 3.5)
    #[allow(dead_code)]
    #[error("Stream timeout: {0}")]
    StreamTimeout(String),

    /// v0.11.0: Buffer overflow — SSE buffer exceeded safety limit
    /// Tracking: Future integration for buffer overflow handling (PHASE 3.5)
    #[allow(dead_code)]
    #[error("Buffer overflow: {0}")]
    BufferOverflow(String),
}

/// Map HTTP status to Anthropic-native error type.
/// CC uses these types to decide retry behaviour:
///   - "rate_limit_error" → CC retries internally
///   - "overloaded_error" → CC retries internally
///   - "api_error" → CC retries internally
///   - Others → CC shows error to user
pub(crate) fn anthropic_error_type(status: StatusCode) -> &'static str {
    match status.as_u16() {
        400 => "invalid_request_error",
        401 => "authentication_error",
        402 => "billing_error",
        403 => "permission_error",
        404 => "not_found_error",
        413 => "request_too_large",
        429 => "rate_limit_error",
        500 | 502 | 504 => "api_error",
        503 | 529 => "overloaded_error",
        _ => "api_error",
    }
}

impl IntoResponse for ProxyError {
    fn into_response(self) -> Response {
        let (status, error_message) = match self {
            ProxyError::Config(msg) => (StatusCode::INTERNAL_SERVER_ERROR, msg),
            ProxyError::Transform(msg) => (StatusCode::BAD_REQUEST, msg),
            ProxyError::Upstream(msg) => (StatusCode::BAD_GATEWAY, msg),
            ProxyError::Overloaded(msg) => (StatusCode::SERVICE_UNAVAILABLE, msg),
            ProxyError::Serialization(err) => {
                (StatusCode::BAD_REQUEST, format!("JSON error: {}", err))
            }
            ProxyError::Http(err) => (StatusCode::BAD_GATEWAY, format!("HTTP error: {}", err)),
            ProxyError::Internal(msg) => (StatusCode::INTERNAL_SERVER_ERROR, msg),
            ProxyError::WebFetch(msg) => (StatusCode::BAD_GATEWAY, msg),
            // v6.1: 400 so CC treats it as non-retriable → immediate /compact feedback
            ProxyError::ContextOverflow(msg) => (StatusCode::BAD_REQUEST, msg),
            // v0.11.0: Stream timeout → 504 Gateway Timeout (retryable)
            ProxyError::StreamTimeout(msg) => (StatusCode::GATEWAY_TIMEOUT, msg),
            // v0.11.0: Buffer overflow → 502 Bad Gateway (retryable)
            ProxyError::BufferOverflow(msg) => (StatusCode::BAD_GATEWAY, msg),
        };

        // Anthropic-native response format — CC recognizes these error types
        let body = Json(json!({
            "type": "error",
            "error": {
                "type": anthropic_error_type(status),
                "message": error_message,
            }
        }));

        (status, body).into_response()
    }
}

/// Result type for proxy operations
pub type ProxyResult<T> = Result<T, ProxyError>;

#[cfg(test)]
#[path = "error_test.rs"]
mod error_test;
