//! Edit Audit Logger — Prometheus metrics for Edit tool outcomes
//!
//! Tracks Edit success/failure rates, error types, latency, and rescue
//! effectiveness. Exposed via /metrics endpoint.

use metrics::{counter, histogram};
use std::path::Path;
use std::time::Duration;

/// Outcome of an Edit tool operation
#[allow(dead_code)] // TODO: Wire into streaming.rs pipeline (Issue #88)
#[derive(Debug, Clone)]
pub enum EditOutcome {
    Success,
    Failed(EditFailureType),
    Rescued,
}

/// Classification of Edit failure types
#[allow(dead_code)] // TODO: Wire into streaming.rs pipeline (Issue #88)
#[derive(Debug, Clone)]
pub enum EditFailureType {
    StringNotFound,
    NotUnique,
    ModifiedSinceRead,
    InputValidation,
    SameString,
    UnicodeMismatch,
}

impl EditFailureType {
    #[allow(dead_code)] // TODO: Wire into streaming.rs pipeline (Issue #88)
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::StringNotFound => "string_not_found",
            Self::NotUnique => "not_unique",
            Self::ModifiedSinceRead => "modified_since_read",
            Self::InputValidation => "input_validation",
            Self::SameString => "same_string",
            Self::UnicodeMismatch => "unicode_mismatch",
        }
    }
}

/// Records an Edit outcome with Prometheus metrics.
#[allow(dead_code)] // TODO: Wire into streaming.rs pipeline (Issue #88)
pub fn record_edit_outcome(result: &EditOutcome, file_path: &str, latency: Duration) {
    let file_basename =
        Path::new(file_path).file_name().and_then(|n| n.to_str()).unwrap_or("unknown");

    match result {
        EditOutcome::Success => {
            counter!("nexus_edit_total", "result" => "success".to_string()).increment(1);
        }
        EditOutcome::Failed(reason) => {
            counter!("nexus_edit_total", "result" => "failure".to_string()).increment(1);
            counter!("nexus_edit_failure_type", "type" => reason.as_str().to_string()).increment(1);
        }
        EditOutcome::Rescued => {
            counter!("nexus_edit_total", "result" => "rescued".to_string()).increment(1);
            counter!("nexus_edit_rescue_total", "result" => "rescued".to_string()).increment(1);
        }
    }

    counter!("nexus_edit_file_path", "file" => file_basename.to_string()).increment(1);
    histogram!("nexus_edit_latency_seconds").record(latency.as_secs_f64());
}

/// Records an Edit rescue attempt outcome.
#[allow(dead_code)] // TODO: Wire into streaming.rs pipeline (Issue #88)
pub fn record_edit_rescue_outcome(outcome: &str) {
    counter!("nexus_edit_rescue_total", "result" => outcome.to_string()).increment(1);
}

/// Classifies an Edit error message into its failure type.
#[allow(dead_code)] // TODO: Wire into streaming.rs pipeline (Issue #88)
pub fn classify_edit_error(error_msg: &str) -> EditFailureType {
    if error_msg.contains("not unique") {
        EditFailureType::NotUnique
    } else if error_msg.contains("String to replace not found") {
        // Check for Unicode-related mismatch indicators
        // Look for escaped Unicode sequences (\uXXXX) which indicate
        // Claude Code normalized Unicode in old_string.
        // NOTE: Do NOT check for "->" here — it's too broad and matches
        // normal Rust syntax (fn signatures, match arms, etc.).
        // Nuanced detection of sanitized "->" vs "->" belongs in
        // edit_rescue.rs during fuzzy matching, not here.
        let has_unicode_markers = error_msg.contains("\\u")
            || error_msg.contains("[TIMEOUT]")
            || error_msg.contains("[WARN]");
        if has_unicode_markers {
            EditFailureType::UnicodeMismatch
        } else {
            EditFailureType::StringNotFound
        }
    } else if error_msg.contains("modified since read") {
        EditFailureType::ModifiedSinceRead
    } else if error_msg.contains("InputValidationError") {
        EditFailureType::InputValidation
    } else if error_msg.contains("exactly the same") {
        EditFailureType::SameString
    } else {
        EditFailureType::StringNotFound // default
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_classify_string_not_found() {
        let result = classify_edit_error("String to replace not found: foo");
        assert!(matches!(result, EditFailureType::StringNotFound));
    }

    #[test]
    fn test_classify_not_unique() {
        let result = classify_edit_error("old_string is not unique in file");
        assert!(matches!(result, EditFailureType::NotUnique));
    }

    #[test]
    fn test_classify_modified_since_read() {
        let result = classify_edit_error("file has been modified since read");
        assert!(matches!(result, EditFailureType::ModifiedSinceRead));
    }

    #[test]
    fn test_classify_input_validation() {
        let result = classify_edit_error("InputValidationError: old_string is empty");
        assert!(matches!(result, EditFailureType::InputValidation));
    }

    #[test]
    fn test_classify_same_string() {
        let result = classify_edit_error("old_string and new_string are exactly the same");
        assert!(matches!(result, EditFailureType::SameString));
    }

    #[test]
    fn test_classify_unicode_mismatch() {
        let result = classify_edit_error("String to replace not found: \\u2192 result");
        assert!(matches!(result, EditFailureType::UnicodeMismatch));
    }

    #[test]
    fn test_failure_type_as_str() {
        assert_eq!(EditFailureType::StringNotFound.as_str(), "string_not_found");
        assert_eq!(EditFailureType::NotUnique.as_str(), "not_unique");
        assert_eq!(EditFailureType::ModifiedSinceRead.as_str(), "modified_since_read");
        assert_eq!(EditFailureType::InputValidation.as_str(), "input_validation");
        assert_eq!(EditFailureType::SameString.as_str(), "same_string");
        assert_eq!(EditFailureType::UnicodeMismatch.as_str(), "unicode_mismatch");
    }
}
