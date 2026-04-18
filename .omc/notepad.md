# Notepad
<!-- Auto-managed by OMC. Manual edits preserved in MANUAL section. -->

## Priority Context
<!-- ALWAYS loaded. Keep under 500 chars. Critical discoveries only. -->

## Working Memory
<!-- Session notes. Auto-pruned after 7 days. -->
### 2026-04-17 06:19
NEXUS-AI-Gateway Performance Analysis completed
- CRITICAL: web_fetch.rs Regex::new() in hot path (strip_html_tags) - compiles regex on every call
- CRITICAL: tokenizer.rs already using cl100k_base_singleton() with PERF FIX comment - already optimized
- HIGH: Gated regex compilation in proxy.rs lines 175-176, 378 - compiled on every error extraction
- HIGH: String allocation patterns in tokenizer.rs collect_request_text - multiple to_string() calls
- MEDIUM: serde_json::to_string_pretty in verbose mode only (proxy.rs lines 861, 901, 971, 980)
- HTTP client already configured with pool_max_idle_per_host(10)


## MANUAL
<!-- User content. Never auto-pruned. -->

