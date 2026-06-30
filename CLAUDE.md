# CLAUDE.md This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Development Commands

```bash
# Build
cargo build --release          # Production build (~7 MB binary)
cargo build                    # Debug build

# Testing
cargo test                     # Run all tests
cargo test tokenizer_test      # Run specific test module by path
cargo test -- --nocapture      # Show println! output
cargo test --test integration  # Integration tests only (wiremock)

# Linting & Formatting
cargo fmt --check              # Check formatting
cargo clippy -- -D warnings   # Lint (warnings as errors)

# Security Audit
cargo audit                    # Check for CVEs in dependencies

# Task Runner (via Taskfile.yaml — preferred)
task check                     # Full check: fmt + lint + test
task test                      # Run all tests
task version-check             # Verify VERSION/Cargo.toml/lib.rs sync
task install                   # Build + install to ~/.cargo/bin
task sync-binary               # Install to ~/.cargo/bin AND ~/.local/bin with md5 verify
task setup                     # Full project setup (build, hooks, service)
task service-status            # Check systemd service status
task service-logs              # Follow service logs
task bump-patch                # Bump PATCH version
task bump-minor                # Bump MINOR version
task auto-version              # Auto-detect bump from conventional commits
task full-release              # One-command release (auto-bump + commit + tag + push)
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
├─ classify::classify_error (L1=status, L2=pattern, L3=structural)
└─ circuit_breaker::CircuitBreaker (optional)
↓
Upstream OpenAI-compatible API
↓
SSE Stream → transform back to Anthropic format
```

### Module Organization

**Proxy layer** — `src/proxy/` (~2382 LOC total, decomposed from monolithic proxy.rs):

| Module | Purpose |
|--------|---------|
| `mod.rs` | Request handler, validation, metrics capture |
| `streaming.rs` | SSE streaming with anthropic.keep-alive |
| `non_streaming.rs` | Synchronous response handling |
| `retry.rs` | 3-layer retry with exponential backoff |
| `classify.rs` | Error classification (L1/L2/L3) |
| `rate_limit.rs` | Rate limit detection |
| `concurrency.rs` | Per-model semaphores, circuit breaker |
| `discovery.rs` | Model capability probing with caching |
| `overflow_tracker.rs` | Context window overflow tracking |
| `token_scaling.rs` | Token scaling between upstream context and CC's context window |
| `headers.rs` | Client header extraction (`anthropic-beta`, `anthropic-version`) and resolution for Anthropic upstreams |
| `error_types.rs` | Upstream error structures |

**Models layer** — `src/models/` (API type definitions):

| Module | Purpose |
|--------|---------|
| `anthropic.rs` | Anthropic request/response types, SSE events, Usage with cache token fields |
| `openai.rs` | OpenAI-compatible request/response types |
| `mod.rs` | Re-exports |

**Other modules** — `src/` root:

| Module | Purpose |
|--------|---------|
| `transform.rs` | Bidirectional Anthropic↔OpenAI conversion, `clean_schema`, thinking sanitization |
| `config.rs` | Config loading from env/.env, hot-reload, `SharedConfig = Arc<ArcSwap<Config>>` |
| `circuit_breaker.rs` | Circuit breaker (Closed/Open/HalfOpen states) — currently default-off |
| `prompt_cache.rs` | SHA-256 content hashing, TTL+LRU proxy-side cache (for NIM KV-cache reuse) |
| `tokenizer.rs` | Token estimation via tiktoken cl100k_base |
| `web_fetch.rs` | WebFetch tool interception — fetches URLs locally, strips HTML |
| `scan.rs` | NIM model discovery via `/v1/models` endpoint |
| `setup.rs` | First-time setup wizard |
| `str_utils.rs` | UTF-8 safe string truncation (prevents panic on multi-byte chars) |
| `watcher.rs` | File watcher for .env hot-reload |
| `cli.rs` | CLI argument parsing (clap) |
| `telemetry/` | Privacy-first telemetry (HMAC fingerprinting, SQLite analytics, daily beacon) |

### Key Dependencies

- `axum 0.7` — HTTP server + middleware
- `tokio 1.42` — Async runtime
- `reqwest 0.12` — HTTP client (pool_max_idle_per_host: 50, tcp_keepalive: 60s)
- `arc-swap 1.x` — Lock-free config reads via `SharedConfig = Arc<ArcSwap<Config>>`
- `tiktoken-rs 0.11` — Token estimation using cl100k_base
- `metrics-exporter-prometheus 0.18` — Prometheus metrics endpoint
- `clap 4` — CLI argument parsing
- `dotenvy` — .env file loading

## Critical Design Conventions

These behaviors are intentional and should not be changed:

### Transform Layer (src/transform.rs)

1. **`has_thinking = true` BY DESIGN** — NIM upstreams force `enable_thinking=true` via `chat_template_kwargs` to produce better output with thinking enabled globally, not just for Opus. Non-NIM upstreams (Anthropic, OpenAI, OpenRouter) handle thinking natively and do not receive `chat_template_kwargs`.

2. **Model identity preservation BY DESIGN** — Responses return the original Claude model ID (e.g., `claude-sonnet-4-6`) even when routed to different upstream models. This is done via `original_model` parameter in streaming responses.

3. **`anthropic-beta` header conditional BY DESIGN** — Only sent to Anthropic upstream (when `NEXUS_UPSTREAM_TYPE=anthropic` or per-upstream `UPSTREAM_<NAME>_TYPE=anthropic`). Never sent to NIM/OpenAI/OpenRouter. Client betas are merged with `PROXY_MINIMUM_BETAS` (e.g. `prompt-caching-scope-2026-01-05`) and deduplicated. When client omits `anthropic-beta`, only proxy minimums are sent.

4. **`chat_template_kwargs` conditional BY DESIGN** — `enable_thinking=true` via `chat_template_kwargs` is only included when the upstream type is NIM. Non-NIM upstreams (Anthropic, OpenAI, OpenRouter) receive the request without `chat_template_kwargs`, since they handle thinking natively.

### Proxy Layer (src/proxy/)

5. **Context overflow threshold BY DESIGN** — Default 90% (configurable via `CC_OVERFLOW_THRESHOLD_PCT`, clamped to 50-95). Requests exceeding context window are pre-checked and clamped before upstream calls.

6. **`probe_model_limit` capability discovery BY DESIGN** — Models without known limits are probed at runtime with a test request. Results cached in `ModelCache` (TTL from `PROBE_CACHE_TTL_SECS`).

7. **`anthropic.keep-alive` SSE event BY DESIGN** — Streaming sends periodic `anthropic.keep-alive` events (30s interval) to prevent Claude Code timeout on slow upstreams.

8. **Token scaling alignment BY DESIGN** — `scale_token_usage()` in `token_scaling.rs` scales both `input_tokens` and `output_tokens` proportionally when upstream context < CC context (Branch 1). When upstream >= CC context (Branch 2), real tokens are reported — CC manages its own window. The `resolve_cc_context_window()` function subtracts `min(max_tokens, 20000)` system overhead (matching CC binary `Pd()`) so the proxy's overflow threshold (default 90%) aligns with CC's auto-compact trigger (~167K for opus-4-6).

### Config (src/config.rs)

9. **SharedConfig = Arc<ArcSwap<Config>> BY DESIGN** — Lock-free reads via `arc_swap`. Hot-reload works by storing new Arc in ArcSwap; no RwLock poisoning possible.

10. **Config reload serialization BY DESIGN** — SIGHUP and file watcher reloads are serialized via `AtomicBool` compare_exchange to prevent race conditions.

11. **No Anthropic API key validation BY DESIGN** — Any non-empty key is accepted. Gateway validates upstream credentials only.

12. **Telemetry always-on BY DESIGN** — Telemetry is enabled by default (v0.19.0+). Local SQLite analytics with HMAC-SHA256 fingerprinting (instance-specific secret) runs without configuration. Daily beacon to CF Worker sends only aggregated stats (zero PII). Users can disable with `TELEMETRY_ENABLED=false` or `TELEMETRY_BEACON_URL=""`.

13. **`*_FILE` secret resolution BY DESIGN** (Issue #115) — `resolve_secret()` lets any API key (`UPSTREAM_API_KEY`, `OPENROUTER_API_KEY`, `UPSTREAM_BIGMODEL_API_KEY`, `UPSTREAM_CF_API_KEY`) be loaded from a file via a `*_FILE` sibling. Precedence: non-empty direct value > trimmed file contents > `None`. Empty/unreadable files warn and fall through (never abort — another source may cover). Both load paths (`from_map` hot-reload, `from_env_with_path` startup) call it, so behavior is identical on startup and reload. Uses `eprintln!` (config built before tracing exists) and never logs the secret — only the path.

## Key Environment Variables

| Variable | Default | Purpose |
|----------|---------|---------|
| `UPSTREAM_BASE_URL` | (required) | Upstream API endpoint URL |
| `UPSTREAM_API_KEY` | (required) | API key for upstream service. Also loadable from a file via `UPSTREAM_API_KEY_FILE` (Issue #115) |
| `<KEY>_FILE` | (none) | Load any API key from a file (trimmed contents). Applies to `UPSTREAM_API_KEY`, `OPENROUTER_API_KEY`, `UPSTREAM_BIGMODEL_API_KEY`, `UPSTREAM_CF_API_KEY`. Direct value wins when both set; empty/unreadable file warns and is ignored. Same behavior at startup and on hot-reload |
| `NEXUS_UPSTREAM_TYPE` | `nim` | Upstream type: `anthropic`, `nim`, `openai`, `openrouter` |
| `UPSTREAM_<NAME>_TYPE` | (falls back to `NEXUS_UPSTREAM_TYPE`) | Per-upstream type override. `<NAME>` matches the upstream name (e.g., `UPSTREAM_BIGMODEL_TYPE=anthropic`). Overrides global `NEXUS_UPSTREAM_TYPE` for that upstream |
| `PORT` | `8315` | Server port |
| `BIND_ADDR` | `127.0.0.1` | Listener bind address (Issue #78). `0.0.0.0` exposes on all interfaces (opt-in; warns when non-loopback). Legacy `HOST` is deprecated/ignored. Overridable via `--bind` |
| `ALLOWED_IPS` | (none) | Optional comma-separated CIDR/IP allowlist (defense-in-depth). Empty = allow all; loopback always allowed |
| `DEBUG` | `false` | Enable debug logging |
| `VERBOSE` | `false` | Full request/response logging |
| `CC_CONTEXT_WINDOW` | `200000` | Context window size for auto-compact calibration |
| `CC_OVERFLOW_THRESHOLD_PCT` | `90` | Context overflow threshold (50-95%) |
| `PROBE_CACHE_TTL_SECS` | `3600` | Model capability probe cache TTL |
| `DISABLE_PROBING` | `false` | Disable runtime model probing |
| `MODEL_LIMIT_OVERRIDES` | (none) | Override model context limits: `model_id:tokens` |
| `CORS_ALLOWED_ORIGINS` | `*` | Comma-separated allowed CORS origins |
| `NIM_PROMPT_CACHE_ENABLED` | `false` | Enable proxy-side prompt cache for NIM |
| `DRAIN_TIMEOUT_SECS` | `30` | Max graceful drain duration before forced shutdown |
| `UPSTREAM_FIRST_BYTE_TIMEOUT_SECS` | `60` | First-byte (response-headers) timeout (Issue #83). Aborts a stalled upstream that accepts the connection but never responds, instead of hanging Claude Code indefinitely — `read_timeout` only fires AFTER the first byte. Wraps `send()` in both retry paths; must be > 0 |
| `FALLBACK_MODELS` | (none) | Comma-separated, priority-ordered upstream model ids (Issue #67). When the primary model fails with a non-transient signal — 5xx, model-not-found/EOL (404/410), or a first-byte stall — NEXUS swaps to the next model here (same upstream) instead of giving up, so one dead model can't take down every session mapped to it. Max 2 fallbacks; **not** triggered on 429 (rate limit, retried) or 401/403 (auth) |
| `CHUNK_TIMEOUT_SECS` | `120` | Inter-chunk (between-SSE-event) timeout for streaming. If the upstream sends no event for this long the stream is closed with an error. Enforced by the streaming select-loop (keep-alives are still emitted to CC meanwhile) |
| `THINKING_MODEL_CHUNK_TIMEOUT_SECS` | `300` | Longer inter-chunk timeout for "thinking" models (Issue #79). Heavy reasoners (glm-5, deepseek-r1, qwq, o1/o3) go fully SILENT during reasoning — NIM emits no SSE at all — so the normal 120s would cut them off mid-thought and waste the whole (60K+ token) request on a retry. `is_thinking_model()` detects them by substring; the client `read_timeout` backstop is raised to `max(CHUNK_TIMEOUT_SECS, this)` so the per-model streaming timeout is the real limit |
| `TELEMETRY_ENABLED` | `true` | Master switch — set `false` to disable all telemetry |
| `TELEMETRY_BEACON_URL` | `https://nexus-beacon-receiver.enerby212.workers.dev/v1/beacon` | Beacon endpoint URL. Set to empty string to disable beacon only |
| `BEACON_AUTH_TOKEN` | (compiled in) | Auth token for beacon endpoint. Override via env var if needed |
| `TELEMETRY_RETENTION_DAYS` | `30` | Days before auto-purge of local analytics data |

Model mapping: `MODEL_MAP_<claude_id_with_underscores>=<upstream>:<model>` (hyphens → underscores in model IDs)
Per-upstream type: `UPSTREAM_<NAME>_TYPE=anthropic|nim|openai|openrouter` — overrides global `NEXUS_UPSTREAM_TYPE` for a named upstream

## Testing Strategy

- **Unit tests**: In `#[cfg(test)]` modules within source files
- **Integration tests**: `tests/integration_test.rs` using wiremock
- **Version sync tests**: `tests/version_sync.rs` validates 3-file sync

Run specific module tests:
```bash
cargo test tokenizer_test        # src/tokenizer_test.rs
cargo test validation_tests      # src/proxy/mod.rs::validation_tests
cargo test threshold_tests       # src/proxy/mod.rs::threshold_tests
cargo test error_test            # src/error_test.rs
cargo test --test integration    # tests/integration_test.rs only
```

## Version Management

Three files must stay synchronized:
- `VERSION` — Single line version string
- `Cargo.toml` — `version = "X.Y.Z"`
- `src/lib.rs` — `pub const VERSION: &str = "X.Y.Z"`

Run `task version-check` to verify. CI enforces this on every push.

## Git Hooks & CI

Hooks are in `scripts/hooks/` (portable, not in `.git/hooks/`):
- **pre-commit**: `cargo fmt --check`
- **commit-msg**: Conventional commits format validation
- **pre-push**: Version sync + `cargo test` + `cargo clippy` + `cargo audit` (main branch only)
- **post-merge**: Auto-rebuild + install + restart systemd service (main branch only, version change detected)

Setup: `task setup-hooks` or `bash scripts/setup-hooks.sh`

CI: `.github/workflows/ci.yml` → `.github/workflows/auto-version.yml` (auto-bumps version on conventional commits, creates GitHub Release with binary)

## API Endpoints

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/v1/messages` | POST | Main proxy endpoint (Anthropic Messages API) |
| `/v1/messages/count_tokens` | POST | Token count estimation |
| `/health` | GET | Health check (`200 OK` normal, `503` during drain) |
| `/metrics` | GET | Prometheus metrics |

## Port Convention

Default port **8315**: N(78) + E(69) + U(85) + S(83) = 315 → 8315

## Documentation Repo

Documentation, audit reports, and issue tracking are in a **separate private repo**: `enerBydev/nexus-ai-gateway_docs` (auto-synced via `nexus-docs-sync.sh` — auto-commit on changes, daily push at 00:00).
