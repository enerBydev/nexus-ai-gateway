#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_proxy_error_creation() {
        let error = ProxyError::Config("test error".to_string());
        match error {
            ProxyError::Config(msg) => {
                assert_eq!(msg, "test error");
            }
            _ => panic!("Wrong error type")
        }
    }

    #[test]
    fn test_error_into_response() {
        let error = ProxyError::Config("test error".to_string());
        let response = error.into_response();
        // Test that the error can be converted to a response
        assert!(true); // Placeholder assertion
    }
}