# CLAUDE.md
This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Development Commands

```bash
# Build
cargo build --release          # Production build (~7 MB binary)
cargo build                    # Debug build

# Testing
cargo test                     # Run all tests (95 unit + 4 integration + 3 version sync)
cargo test tokenizer_test      # Run specific test module by path
cargo test -- --nocapture      # Show println! output

# Linting & Formatting
cargo fmt --check              # Check formatting
cargo clippy -- -D warnings    # Lint (warnings as errors)

# Security Audit
cargo audit                    # Check for CVEs in dependencies

# Task Runner (via Taskfile.yaml)
task test                      # Run all tests
task version-check             # Verify VERSION/Cargo.toml/lib.rs sync
task check                     # Run fmt-check + lint + test
task install                   # Build and install to ~/.cargo/bin
```

## Architecture

### Request Flow

```
Claude Code → POST /v1/messages
                    ↓
              proxy::proxy_handler (src/proxy/mod.rs)
                    ↓
              ├─ validate_request (pre-flight checks)
              ├─ transform::anthropic_to_openai (bidirectional conversion)
              ├─ discovery::get_context_limit (caches model capabilities)
              ├─ tokenizer::estimate_from_openai_request (tiktoken pre-check)
              └─ concurrency::ModelSemaphores (per-model rate limiting)
                    ↓
         streaming::handle_streaming OR non_streaming::handle_non_streaming
                    ↓
              ├─ retry::execute_with_retry (3-layer classification)
              ├─ classify::classify_error (L1/L2/L3 error detection)
              └─ circuit_breaker::CircuitBreaker (optional)
                    ↓
              Upstream OpenAI-compatible API
                    ↓
              SSE Stream → transform back to Anthropic format
```

### Module Organization

The `src/proxy/` directory contains 10 modules (~2382 LOC total) decomposed from the original monolithic proxy.rs:

| Module | Purpose |
|--------|---------|
| `mod.rs` | Request handler, validation, metrics capture |
| `streaming.rs` | SSE streaming with anthropic.keep-alive |
| `non_streaming.rs` | Synchronous response handling |
| `retry.rs` | 3-layer retry with exponential backoff |
| `classify.rs` | Error classification (L1=status, L2=pattern, L3=structural) |
| `rate_limit.rs` | Rate limit detection (L1/L2/L3) |
| `concurrency.rs` | Per-model semaphores, circuit breaker |
| `discovery.rs` | Model capability probing with caching |
| `overflow_tracker.rs` | Context window overflow tracking |
| `error_types.rs` | Upstream error structures |

### Key Dependencies

- `axum 0.7` — HTTP server + middleware
- `tokio 1.42` — Async runtime
- `reqwest 0.12` — HTTP client (pool_max_idle_per_host: 50, tcp_keepalive: 60s)
- `arc-swap 1.x` — Lock-free config reads via SharedConfig = Arc<ArcSwap<Config>>
- `tiktoken-rs 0.11` — Token estimation using cl100k_base
- `metrics-exporter-prometheus 0.18` — Prometheus metrics endpoint

## Critical Design Conventions

These behaviors are intentional and should not be changed:

### Transform Layer (src/transform.rs)

1. **`has_thinking = true` BY DESIGN** — All requests force `enable_thinking=true` via `chat_template_kwargs`. NIM models produce better output with thinking enabled globally, not just for Opus.

2. **Model identity preservation BY DESIGN** — Responses return the original Claude model ID (e.g., `claude-sonnet-4-6`) even when routed to different upstream models. This is done via `original_model` parameter in streaming responses.

3. **`anthropic-beta` header conditional BY DESIGN** — Only sent to Anthropic upstream (when `NEXUS_UPSTREAM_TYPE=anthropic`). Never sent to NIM/OpenAI/OpenRouter.

### Proxy Layer (src/proxy/)

4. **Context overflow threshold BY DESIGN** — Default 80% (configurable via `CC_OVERFLOW_THRESHOLD_PCT`, clamped to 50-95). Requests exceeding context window are pre-checked and clamped before upstream calls.

5. **`probe_model_limit` capability discovery BY DESIGN** — Models without known limits are probed at runtime with a test request. Results cached in `ModelCache` (TTL from `PROBE_CACHE_TTL_SECS`).

6. **`anthropic.keep-alive` SSE event BY DESIGN** — Streaming sends periodic `anthropic.keep-alive` events (30s interval) to prevent Claude Code timeout on slow upstreams.

### Config (src/config.rs)

7. **SharedConfig = Arc<ArcSwap<Config>> BY DESIGN** — Lock-free reads via `arc_swap`. Hot-reload works by storing new Arc in ArcSwap; no RwLock poisoning possible.

8. **Config reload serialization BY DESIGN** — SIGHUP and file watcher reloads are serialized via `AtomicBool` compare_exchange to prevent race conditions.

9. **No Anthropic API key validation BY DESIGN** — Any non-empty key is accepted. Gateway validates upstream credentials only.

## Testing Strategy

- **Unit tests**: In `#[cfg(test)]` modules within source files (95 tests)
- **Integration tests**: `tests/integration_test.rs` using wiremock (4 tests)
- **Version sync tests**: `tests/version_sync.rs` validates 3-file sync (3 tests)

Run specific module tests:
```bash
cargo test tokenizer_test      # src/tokenizer_test.rs
cargo test validation_tests    # src/proxy/mod.rs::validation_tests
cargo test threshold_tests     # src/proxy/mod.rs::threshold_tests
cargo test error_test          # src/error_test.rs
cargo test --test integration  # tests/integration_test.rs only
```

## Version Management

Three files must stay synchronized:
- `VERSION` — Single line version string
- `Cargo.toml` — `version = "X.Y.Z"`
- `src/lib.rs` — `pub const VERSION: &str = "X.Y.Z"`

Run `task version-check` to verify. CI enforces this on every push.

## API Endpoints

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/v1/messages` | POST | Main proxy endpoint (Anthropic Messages API) |
| `/v1/messages/count_tokens` | POST | Token count estimation |
| `/health` | GET | Health check (returns `OK`) |
| `/metrics` | GET | Prometheus metrics |

## Port Convention

Default port **8315**: N(78) + E(69) + U(85) + S(83) = 315 → 8315
