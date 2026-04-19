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

## [0.12.0] - 2026-04-19

### Added
- Circuit breaker module for upstream request protection (prevents cascade failures)
- L2 rate limit detection system for provider-side concurrency caps
- Configurable CC_CONTEXT_WINDOW environment variable for auto-compact scaling
- Partial flush optimization for streaming (100ms timeout, 512 byte minimum)
- Prompt caching header (anthropic-beta: prompt-caching-2024-06-01)

### Changed
- HTTP connection pool increased from 10 to 50 for multi-agent scenarios
- HTTP/2 enabled for better multiplexing
- Added tcp_nodelay and tcp_keepalive for connection health
- Enhanced error logging with structured metrics

### Fixed
- Prevented cascade timeouts on rate limit errors
- Reduced latency in streaming with partial flush
- Missing cache activation header added to all requests

### Technical Details
- Pool idle timeout set to 30 seconds for faster connection release
- HTTP/2 prior knowledge mode enabled
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