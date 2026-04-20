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
- Prevented cascade timeouts on rate limit errors
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