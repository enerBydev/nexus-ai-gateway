use super::*;
use serde_json::json;

#[test]
fn estimate_simple_message() {
    let req = json!({
        "messages": [{"role": "user", "content": "Hello world"}]
    });
    let tokens = estimate_input_tokens(&req);
    assert!(tokens > 0, "Simple message should produce > 0 tokens");
    assert!(tokens < 20, "Simple message should produce < 20 tokens, got {}", tokens);
}

#[test]
fn estimate_with_system_prompt() {
    let without_system = json!({
        "messages": [{"role": "user", "content": "Hello world"}]
    });
    let with_system = json!({
        "system": "You are a helpful AI assistant that excels at coding tasks.",
        "messages": [{"role": "user", "content": "Hello world"}]
    });
    let tokens_without = estimate_input_tokens(&without_system);
    let tokens_with = estimate_input_tokens(&with_system);
    assert!(
        tokens_with > tokens_without,
        "System prompt should increase token count: {} vs {}",
        tokens_with,
        tokens_without
    );
}

#[test]
fn estimate_with_tools() {
    let without_tools = json!({
        "messages": [{"role": "user", "content": "Hello"}]
    });
    let with_tools = json!({
        "messages": [{"role": "user", "content": "Hello"}],
        "tools": [
            {
                "type": "function",
                "function": {
                    "name": "read_file",
                    "description": "Read the contents of a file from the filesystem",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "path": {"type": "string", "description": "Absolute path to the file"}
                        },
                        "required": ["path"]
                    }
                }
            },
            {
                "type": "function",
                "function": {
                    "name": "write_file",
                    "description": "Write content to a file on the filesystem",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "path": {"type": "string"},
                            "content": {"type": "string"}
                        },
                        "required": ["path", "content"]
                    }
                }
            },
            {
                "type": "function",
                "function": {
                    "name": "run_command",
                    "description": "Execute a shell command and return output",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "command": {"type": "string", "description": "The command to execute"}
                        },
                        "required": ["command"]
                    }
                }
            }
        ]
    });
    let tokens_without = estimate_input_tokens(&without_tools);
    let tokens_with = estimate_input_tokens(&with_tools);
    assert!(
        tokens_with > tokens_without + 50,
        "Tools should add significant tokens: {} vs {} (diff={})",
        tokens_with,
        tokens_without,
        tokens_with - tokens_without
    );
}

#[test]
fn estimate_empty_input() {
    let req = json!({});
    let tokens = estimate_input_tokens(&req);
    assert_eq!(tokens, 1, "Empty input should return minimum 1 token");
}

#[test]
fn estimate_large_context() {
    // ~100K chars of text ≈ ~25K tokens
    let large_text = "a ".repeat(50_000); // 100K chars
    let req = json!({
        "messages": [{"role": "user", "content": large_text}]
    });
    let tokens = estimate_input_tokens(&req);
    assert!(tokens > 20_000, "100K chars should produce > 20K tokens, got {}", tokens);
    assert!(tokens < 60_000, "100K chars should produce < 60K tokens, got {}", tokens);
}

#[test]
fn estimate_multipart_content() {
    let single = json!({
        "messages": [{"role": "user", "content": "Hello world"}]
    });
    let multi = json!({
        "messages": [{
            "role": "user",
            "content": [
                {"type": "text", "text": "Hello world"},
                {"type": "text", "text": "This is additional context with more words to count"}
            ]
        }]
    });
    let tokens_single = estimate_input_tokens(&single);
    let tokens_multi = estimate_input_tokens(&multi);
    assert!(
        tokens_multi > tokens_single,
        "Multipart should have more tokens: {} vs {}",
        tokens_multi,
        tokens_single
    );
}

#[test]
fn fallback_produces_nonzero() {
    // Any valid message should always return > 0
    let req = json!({
        "messages": [{"role": "user", "content": "x"}]
    });
    let tokens = estimate_input_tokens(&req);
    assert!(tokens > 0, "Should always produce > 0 tokens");
}

#[test]
fn typed_variant_matches_json_variant() {
    use crate::models::openai::{Message, MessageContent, OpenAIRequest};

    let openai_req = OpenAIRequest {
        model: "test-model".to_string(),
        messages: vec![Message {
            role: "user".to_string(),
            content: Some(MessageContent::Text("What is the capital of France?".to_string())),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        }],
        max_tokens: Some(100),
        temperature: None,
        top_p: None,
        stop: None,
        stream: Some(true),
        stream_options: None,
        tools: None,
        tool_choice: None,
        chat_template_kwargs: None,
        response_format: None,
    };

    let json_req = serde_json::to_value(&openai_req).unwrap();

    let typed_result = estimate_from_openai_request(&openai_req);
    let json_result = estimate_input_tokens(&json_req);

    assert_eq!(
        typed_result, json_result,
        "Typed and JSON variants should return same result: {} vs {}",
        typed_result, json_result
    );
}

#[test]
fn estimate_system_prompt_array() {
    let req = json!({
        "system": [
            {"type": "text", "text": "You are a coding assistant."},
            {"type": "text", "text": "Always use best practices."}
        ],
        "messages": [{"role": "user", "content": "Hello"}]
    });
    let tokens = estimate_input_tokens(&req);
    assert!(tokens > 5, "System array should produce meaningful tokens, got {}", tokens);
}

// ═══════════════════════════════════════════════════════════════════════════════
// v8.0: CalibrationFactors tests
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn calibration_starts_at_1() {
    let cal = CalibrationFactors::new();
    assert!(
        (cal.get("unknown-model") - 1.0).abs() < f64::EPSILON,
        "New model should start with factor 1.0"
    );
    assert_eq!(cal.observation_count("unknown-model"), 0, "New model should have 0 observations");
}

#[test]
fn calibration_adjusts_with_feedback() {
    let cal = CalibrationFactors::new();
    // tiktoken says 1000, NIM says 1100 -> observed ratio = 1.1
    cal.update("test-model", 1000, 1100);

    let factor = cal.get("test-model");
    // With alpha=0.1: 1.0 * 0.9 + 1.1 * 0.1 = 1.01
    assert!(
        (factor - 1.01).abs() < 0.001,
        "After 1 update with ratio 1.1, factor should be ~1.01, got {}",
        factor
    );
    assert_eq!(cal.observation_count("test-model"), 1);

    // Apply should use the updated factor
    let calibrated = cal.apply("test-model", 1000);
    assert_eq!(calibrated, 1010, "apply(1000) with factor 1.01 should = 1010");
}

#[test]
fn calibration_converges() {
    let cal = CalibrationFactors::new();
    // Simulate 50 requests where NIM always reports 10% more than tiktoken
    for _ in 0..50 {
        cal.update("kimi-k2.5", 1000, 1100);
    }
    let factor = cal.get("kimi-k2.5");
    // After 50 EMA updates with constant ratio 1.1, factor converges to ~1.1
    assert!(
        (factor - 1.1).abs() < 0.01,
        "After 50 updates with constant ratio 1.1, factor should converge to ~1.1, got {:.4}",
        factor
    );
    assert_eq!(cal.observation_count("kimi-k2.5"), 50);

    // Calibrated estimate should now be very close to NIM real
    let calibrated = cal.apply("kimi-k2.5", 1000);
    assert!(
        (calibrated as i64 - 1100).abs() < 15,
        "Calibrated should be ~1100, got {}",
        calibrated
    );
}

#[test]
fn calibration_thread_safe() {
    use std::thread;
    let cal = CalibrationFactors::new();

    // Spawn 10 threads each doing 100 updates
    let mut handles = vec![];
    for i in 0..10 {
        let cal_clone = cal.clone();
        let model = format!("model-{}", i);
        handles.push(thread::spawn(move || {
            for _ in 0..100 {
                cal_clone.update(&model, 1000, 1100 + i * 10);
            }
            cal_clone.get(&model)
        }));
    }

    for (i, handle) in handles.into_iter().enumerate() {
        let factor = handle.join().unwrap();
        let expected_ratio = (1100 + i as u32 * 10) as f64 / 1000.0;
        assert!(
            (factor - expected_ratio).abs() < 0.02,
            "Thread {} factor should converge to {:.2}, got {:.4}",
            i,
            expected_ratio,
            factor
        );
    }
}

#[test]
fn calibration_ignores_zero_values() {
    let cal = CalibrationFactors::new();
    cal.update("test", 0, 1000);
    assert!((cal.get("test") - 1.0).abs() < f64::EPSILON, "Zero tiktoken should not update factor");
    cal.update("test", 1000, 0);
    assert!((cal.get("test") - 1.0).abs() < f64::EPSILON, "Zero NIM should not update factor");
    assert_eq!(cal.observation_count("test"), 0, "No valid observations");
}

#[test]
fn calibration_multi_model_independent() {
    let cal = CalibrationFactors::new();
    // Model A: ratio 1.1
    for _ in 0..20 {
        cal.update("model-a", 1000, 1100);
    }
    // Model B: ratio 0.95
    for _ in 0..20 {
        cal.update("model-b", 1000, 950);
    }
    let a = cal.get("model-a");
    let b = cal.get("model-b");
    assert!(a > 1.05, "Model A should have factor > 1.05, got {:.4}", a);
    assert!(b < 0.98, "Model B should have factor < 0.98, got {:.4}", b);
}
