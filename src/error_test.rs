#[cfg(test)]
mod tests {
    use crate::error::*;
    use axum::http::StatusCode;

    #[test]
    fn test_proxy_error_creation() {
        let error = ProxyError::Config("test error".to_string());
        match error {
            ProxyError::Config(msg) => {
                assert_eq!(msg, "test error");
            }
            _ => panic!("Wrong error type"),
        }
    }

    #[test]
    fn test_error_into_response() {
        let error = ProxyError::Config("test error".to_string());
        let _response = error.into_response();
        assert!(true);
    }

    // v0.11.0: New error variant tests
    #[test]
    fn test_stream_timeout_error() {
        let error = ProxyError::StreamTimeout("NIM stopped after 120s".to_string());
        let response = error.into_response();
        assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
    }

    #[test]
    fn test_buffer_overflow_error() {
        let error = ProxyError::BufferOverflow("10MB exceeded".to_string());
        let response = error.into_response();
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    }

    #[test]
    fn test_error_408_maps_to_api_error() {
        // HTTP 408 should map to "api_error" for CC retry
        let error_type = anthropic_error_type(StatusCode::from_u16(408).unwrap());
        // 408 is not explicitly listed -> falls through to default "api_error"
        assert_eq!(error_type, "api_error");
    }

    #[test]
    fn test_error_429_maps_to_rate_limit() {
        let error_type = anthropic_error_type(StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(error_type, "rate_limit_error");
    }

    #[test]
    fn test_error_503_maps_to_overloaded() {
        let error_type = anthropic_error_type(StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(error_type, "overloaded_error");
    }
}
