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

    #[allow(dead_code)]
    #[error("Internal error: {0}")]
    Internal(String),

    #[error("WebFetch error: {0}")]
    WebFetch(String),
}

/// Map HTTP status to Anthropic-native error type.
/// CC uses these types to decide retry behaviour:
///   - "rate_limit_error" → CC retries internally
///   - "overloaded_error" → CC retries internally
///   - "api_error" → CC retries internally
///   - Others → CC shows error to user
fn anthropic_error_type(status: StatusCode) -> &'static str {
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
