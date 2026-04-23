# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.5.0] - 2026-04-09

### Added
- Initial release of NEXUS-AI-Gateway
- High-performance Anthropic API proxy to OpenAI-compatible endpoints
- Support for streaming responses
- Rate limiting and retry mechanisms
- Environment-based configuration
- Daemon mode support
- Comprehensive observability with tracing

### Changed
- Renamed project from `nexus-brain` to `nexus-ai-gateway`
- Updated binary name in Cargo.toml

### Security
- Added security scanning to CI pipeline
- Implemented proper error handling

## [Unreleased]

### Added

### Changed

### Fixed

---

## [0.13.0] - 2026-04-23

### Added

### Changed

### Fixed

---

## [0.13.0] - 2026-04-22

### Added
- Prompt caching bridge for Anthropic API compatibility
- `anthropic::Usage` extended with 10 cache/token fields (cache_creation_input_tokens, cache_read_input_tokens, CacheCreation, ServerToolUse, etc.)
- Honest zero cache tokens (`Some(0)`) reported for NIM endpoints (NIM doesn't cache)
- Conditional `anthropic-beta: prompt-caching-2024-06-01` header — only sent to Anthropic upstream
- Proxy-side prompt cache module (`src/prompt_cache.rs`)
- SHA-256 content hashing for cache key generation
- TTL-based expiration (default 5 min) + LRU eviction (default 1000 entries)
- Designed for NIM self-hosted with `NIM_ENABLE_KV_CACHE_REUSE=1`
- Cache marker extraction from Anthropic requests
- `CacheMarker` struct: content_hash, token_count, location, cache_control_value
- `TransformResult` struct: request + upstream_name + cache_markers
- Extracts `cache_control: {"type": "ephemeral"}` from system prompts and content blocks
- Cache observability via `tracing::debug!` at cache_control drop points
- New environment variables:
  - `NEXUS_UPSTREAM_TYPE` (anthropic|nim|openai|openrouter, default: nim)
  - `NIM_PROMPT_CACHE_ENABLED` (default: false)
  - `NIM_PROMPT_CACHE_MAX_ENTRIES` (default: 1000)
  - `NIM_PROMPT_CACHE_TTL_SECS` (default: 300)
- 8 new integration tests (58 total, up from 50)
- CacheMarker extraction, TransformResult validation
- Concurrent cache access, bulk operation performance

### Changed
- `anthropic_to_openai()` now returns `TransformResult` instead of `(OpenAIRequest, String)` tuple
- `anthropic-beta` header only sent when `NEXUS_UPSTREAM_TYPE=anthropic`
- Streaming responses include `cache_creation_input_tokens: 0` and `cache_read_input_tokens: 0`

### Fixed
- Missing `anthropic-beta` header prevented cache activation for Anthropic upstream
- Cache token fields absent from streaming message_start, message_delta, and timeout events

---

## [0.12.1] - 2026-04-21

### Added

### Changed

### Fixed

---

## [0.12.0] - 2026-04-20

### Added
- Circuit breaker for upstream request protection with HalfOpen probe limiting (1 probe) and state-aware success recording
- L2 rate limit detection system for provider-side concurrency caps (narrowed patterns to avoid false positives)
- Configurable CC_CONTEXT_WINDOW environment variable for auto-compact scaling
- Prompt caching header (anthropic-beta: prompt-caching-2024-06-01)
- Bidirectional thinking sanitization for `<previous_reasoning` tags (F1)
- WebSearch null schema handling with default valid schema (F3)

### Changed
- HTTP connection pool increased from 10 to 50 for multi-agent scenarios
- HTTP/2 changed from prior knowledge to ALPN negotiation (HTTP/1.1 compatible)
- Added tcp_nodelay and tcp_keepalive for connection health
- Enhanced error logging with structured metrics
- Reload debounce synchronized to 10s with 5s cooldown (prevents burst reloads)
- Stream flush timeout increased to 500ms, minimum flush reduced to 128 bytes (F2)

### Fixed
- Removed partial flush that caused 73.7% of stream decoding errors
- Improved rate-limit detection/backoff groundwork to reduce cascade timeout risk
- Missing cache activation header added to all requests
- L2 rate limit false positives from overly broad "rate limit exceeded" pattern
- Circuit breaker HalfOpen unlimited traffic (now limited to 1 probe)
- Circuit breaker stale success closing circuit inappropriately (now state-aware)

### Technical Details
- Pool idle timeout set to 30 seconds for faster connection release
- HTTP/2 via ALPN negotiation (removed prior knowledge mode for HTTP/1.1 compatibility)
- TCP keepalive set to 60 seconds

---

## [0.11.1] - 2026-04-18

### Added

### Changed

### Fixed

---

## [0.11.0] - 2026-04-18

### Added

### Changed

### Fixed

---

## [0.10.4] - 2026-04-16

### Added

### Changed

### Fixed

---

## [0.10.3] - 2026-04-15

### Added

### Changed

### Fixed

---

## [0.10.2] - 2026-04-15

### Added

### Changed

### Fixed

---

## [0.10.1] - 2026-04-15

### Added

### Changed

### Fixed

---

## [0.10.0] - 2026-04-15

### Added

### Changed

### Fixed

---

## [0.9.0] - 2026-04-14

### Added

### Changed

### Fixed

---

## [0.8.0] - 2026-04-13

### Added

### Changed

### Fixed

---

## [0.7.0] - 2026-04-13

### Added

### Changed

### Fixed

---

## [0.6.2] - 2026-04-13

### Added

### Changed

### Fixed

---

## [0.6.1] - 2026-04-12

### Added

### Changed

### Fixed

---

## [0.6.0] - 2026-04-12

### Added

### Changed

### Deprecated

### Removed

### Fixed

### Security