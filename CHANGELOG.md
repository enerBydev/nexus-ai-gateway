# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- `BIND_ADDR` env var + `--bind` CLI flag — configurable listener bind address (default `127.0.0.1`, loopback-only) (#108, #78)
- `ALLOWED_IPS` env var — optional per-request IP allowlist middleware (defense-in-depth); loopback always allowed (#108)
- `scripts/harden-firewall.sh` — idempotent explicit UFW rule for the proxy port (`--dry-run`, `--allow-lan <cidr>`) (#108)
- systemd `RestrictAddressFamilies` network hardening in `scripts/nexus-ai-gateway.service` (#108)

### Changed
- **Security (bind hardening):** the listener now defaults to `127.0.0.1` (loopback-only) instead of the previously hardcoded `0.0.0.0`. The proxy is no longer reachable from the LAN unless `BIND_ADDR=0.0.0.0` is set explicitly (opt-in), which also logs a warning. Mitigates unauthenticated LAN exposure (#78). The legacy `HOST` variable is deprecated and ignored (warns when set without `BIND_ADDR`).

### Fixed

---

## [0.24.5] - 2026-06-25

### Fixed
- Guard against silent turn death on 0-content completions (#106)

---

## [0.24.4] - 2026-06-24

### Fixed
- Constrain model_tier to real claude ids (#105)
- Family fallback for unmapped Claude model ids (#105)

---

## [0.24.3] - 2026-06-24

### Fixed
- Delay success bookkeeping until after body validation (#119)
- Context headroom + reject impossible requests + robust decode (#107, #119)

---

## [0.24.2] - 2026-06-24

### Changed
- Document intentional sub-MIN_CLAMP floor + regression guard (#62)
- Unify max_tokens clamp decision across both paths (#62)

---

## [0.24.1] - 2026-06-24

### Fixed
- Gate panic backtrace behind capture() (#72)
- Capture backtrace in panic hook (#72)

---

## [0.24.0] - 2026-06-24

### Added
- Support *_FILE convention to load API keys from a file (#115)

### Security
- Support *_FILE convention to load API keys from a file (#115)

---

## [0.23.0] - 2026-06-23

### Added
- DNS-aware SSRF guard in WebFetch (Solution B, #64)

### Changed
- Bump quinn-proto 0.11.14 -> 0.11.15 (RUSTSEC-2026-0185)

### Fixed
- Address CodeRabbit review (PR #114)
- Panic hook + effective systemd crash-loop limit (C, #72)
- Converge WebFetch to execute_fetch + flush at [DONE] (#64/#65/A1/A5)

### Security
- Address CodeRabbit review (PR #114)
- DNS-aware SSRF guard in WebFetch (Solution B, #64)

---

## [0.22.1] - 2026-06-18

### Fixed
- Make bind/HOST config warnings visible at startup (OSS UX)

### Security
- Make bind/HOST config warnings visible at startup (OSS UX)

---

## [0.22.0] - 2026-06-18

### Added
- Default bind to 127.0.0.1 + optional IP allowlist (#108, #78)

### Changed
- Document BIND_ADDR/ALLOWED_IPS + network exposure model (#108, #78)
- Systemd network hardening + explicit firewall script (#108)

### Fixed
- IPv6-safe bind + fail-closed ALLOWED_IPS (CodeRabbit #109)

### Security
- IPv6-safe bind + fail-closed ALLOWED_IPS (CodeRabbit #109)
- Document BIND_ADDR/ALLOWED_IPS + network exposure model (#108, #78)
- Systemd network hardening + explicit firewall script (#108)
- Default bind to 127.0.0.1 + optional IP allowlist (#108, #78)

---

## [0.21.2] - 2026-06-17

### Changed
- Address CodeRabbit nitpicks — log invalid fallback env, strip PORT quotes

### Fixed
- Build release on ubuntu-22.04 + add glibc-safe sync-from-release deploy script
- Probe-fail fallback 200K (not 128K) — stop token inflation that filled context in seconds
- Rate-limit/overload statuses always back off, never clamp (stream death)

---

## [0.21.1] - 2026-06-17

### Fixed
- Post-merge hook shebang to bash (Bad substitution on deploy)

---

## [0.21.0] - 2026-06-16

### Added
- Durable reasoning mode transports thinking as text (#90-B F5)
- Policy-driven reasoning activation, decoupled from model id (#90-B F3+EjeA)
- Distinguish synthetic vs real thinking signatures (#90-B F4)
- FST reasoning transducer + fix sanitize DoS loop (#90-B F2)
- Emit thinking signature_delta with nexus:v1 provenance (#90-B F1)

---

## [0.20.1] - 2026-06-14

### Fixed
- Track local hook symlinks for post-merge auto-install

---

## [0.20.0] - 2026-06-14

### Added
- Sanitize cross-backend tool_use ids (Issue #90 Part A)

### Changed
- Address CodeRabbit review on #91 (injection, mkdir, dev-test guards, changelog errors)
- Thin-LTO release profile + isolated dev-test environment
- Backfill CHANGELOG.md from GitHub Releases + document CB default-off (#44, #36)
- Fix release pipeline — sync Cargo.lock on bump + populate CHANGELOG from commits (#44)
- Remove redundant cargo install rebuild in deploy.sh (#41)

### Fixed
- Wire edit-rescue + edit-metrics into request pipeline (Issue #93)

---

## [0.19.1] - 2026-06-06

### Changed
- Add .repo identity file to .gitignore
- Upgrade Rust toolchain to 1.96, update dependencies

### Fixed
- Address CodeRabbit Round 2 feedback — 1 Major + 1 Minor + 3 outside-diff
- Address CodeRabbit feedback — 3 Major + 2 Minor issues (PR #89)
- Global fix for Issues #63, #74, #80, #60 — 11 interconnected bugs (Issue #88)

---

## [0.19.0] - 2026-06-04

### Added
- Telemetry always-on by default with obfstr protection

---

## [0.18.2] - 2026-06-04

_No release notes recorded._

---

## [0.18.1] - 2026-06-03

_No release notes recorded._

---

## [0.18.0] - 2026-06-03

### Added
- Wire telemetry beacon — daily POST to configured endpoint
- Expand ClientType detection — classify Cline, Aider, Continue, Codex, Cursor, Windsurf, Copilot
- Add privacy-first telemetry module for anonymous usage statistics
- Feat(#85): implement 3-layer autonomous git sync system

### Fixed
- Remove no-op .git/hooks/ entries from .gitignore
- Remove stale #[allow(dead_code)] on telemetry_beacon_url
- Actualizar .gitignore para incluir nuevos patrones de archivos temporales
- Telemetry Phase 0 bug fixes — gauge update, disabled reason, gitignore
- Honor explicit telemetry paths when $HOME is unset
- Address CodeRabbit review — 5 telemetry safety and correctness fixes
- Fix(#85): address CodeRabbit review — 6 safety and correctness fixes

---

## [0.17.4] - 2026-05-30

### Changed
- Chore(#52): trigger CodeRabbit re-review
- Chore(deps): bump the cargo-minor-patch group across 1 directory with 6 updates

### Fixed
- Fix(#52): address CodeRabbit feedback — preserve config_path in reload()
- Fix(#52): eliminate file watcher infinite reload loop with 5-layer protection

---

## [0.17.3] - 2026-05-29

### Fixed
- CodeRabbit review — 3 fixes for PR #82
- Fix(headers): Issue #35 — Anthropic header handling (6 bugs)
- Fix(stream): CR1-CR4 — eliminate stream timeout errors

---

## [0.17.2] - 2026-05-28

### Fixed
- Fix(classify): Issue #34 — redesign error classification with status-code guards
- Fix(emergency): P0 timeout 300s + P2 x-should-retry headers

---

## [0.17.1] - 2026-05-04

### Changed
- Reorder TokenScalingParams + clarify output=0 calls
- Extract token_scaling module + fix P1-P5 (Issue #33)
- Chore(deps): bump the cargo-minor-patch group with 2 updates
- Ignore wiremock >=0.6.5 in dependabot + add scheduled deps monitor workflow

### Fixed
- Resolve 3 audit findings from PR #50 forensic review

### Security
- Resolve 3 audit findings from PR #50 forensic review

---

## [0.17.0] - 2026-04-30

### Added
- Implement graceful shutdown (Issue #30) — close all 12 gaps

### Fixed
- Address CodeRabbit round-2 review — server spawn, portable PID check, cancellable backoff
- Address all 7 CodeRabbit review gaps for graceful shutdown (Issue #30)

---

## [0.16.1] - 2026-04-28

### Fixed
- Sync Cargo.lock + add post-merge hook + refactor pre-push deploy

---

## [0.16.0] - 2026-04-27

### Added
- Dynamic context window sync between Claude Code and upstream models (#31)
- Dynamic context window sync between Claude Code and upstream models

### Fixed
- Address CodeRabbit nitpick feedback — defensive zero filter, warn on invalid config, clarify priority docs
- Fix(test): address CodeRabbit review — RAII EnvGuard + poison-safe mutex

---

## [0.15.1] - 2026-04-27

### Changed
- Add llms.txt for AI assistant context discovery
- Update CLAUDE.md with current architecture and conventions
- Rewrite README.md from scratch + refactor transform.rs cache extraction

### Fixed
- Fix(deps): pin wiremock to 0.6.4 — 0.6.5 requires unstable let_chains

---

## [0.15.0] - 2026-04-27

### Added
- Feat(circuit-breaker): implement CB_ENABLED, CB_THRESHOLD, CB_RECOVERY_SECS env vars

### Changed
- Chore(deps): bump dialoguer 0.12, console 0.16, metrics-exporter-prometheus 0.18
- Ci(dependabot): improve config — groups, limits, labels, reviewers
- Chore(deps): bump all dependencies — rand 0.10, notify 8, wiremock 0.6, indicatif 0.18, CI actions v5-v6

### Fixed
- Fix(deploy): replace cp+chmod with install -m 0755 to avoid ETXTBSY
- Fix(ci): address CodeRabbit review — Taskfile YAML, deploy --locked, CI version-verify
- Fix(deploy): add binary freshness checks to prevent stale binary bug
- Address CodeRabbit review — remove deprecated reviewers, clamp CB params
- Fix(test): add test-only mutex to prevent overflow_tracker race condition
- Fix(circuit-breaker): move record_failure to only Retryable branch + exhausted retries

---

## [0.14.0] - 2026-04-26

### Fixed
- **FIX 4**: Overflow loop detector — tracks consecutive overflow events per model; 3+ identical overflows (within 5% token variation) force `ContextOverflow` error, breaking infinite retry loops
- **FIX 5**: Configurable `ContextOverflow` threshold — `CC_OVERFLOW_THRESHOLD_PCT` env var (default 80%, range 50-95). Replaces hardcoded 90% that was unreachable at 139K tokens with GLM5
- **FIX 6**: Non-streaming `scale_tokens` — mirrors streaming.rs token scaling logic in `non_streaming.rs`, ensuring consistent overflow detection across both request paths

### Added
- `src/proxy/overflow_tracker.rs` — OnceLock+Mutex HashMap-based overflow loop tracking (7 unit tests)
- `get_overflow_threshold_pct()` — configurable threshold with validation (4 unit tests)
- Overflow loop detection in both `resilient_send` (non-streaming) and `resilient_send_raw` (streaming) retry paths

### Changed
- `ContextOverflow` threshold default: 90% → **80%** (CC_OVERFLOW_THRESHOLD_PCT)
- `non_streaming.rs` now receives `context_limit` parameter for token scaling parity
- Both streaming and non-streaming paths now use `scale_tokens` + configurable threshold

### Testing
- 11 new unit tests for FIX 4/5/6
- All 43 existing tests passing
- `cargo clippy -- -D warnings` clean

### Addresses
- Auditoria_v12 C2: Overflow Loop Pattern (CRITICAL)
- Auditoria_v12 FIX 4, 5, 6 requirements
- Cross-validation with Auditoria_v11 root causes RC#1 and RC#2

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

### Fixed
- Fix(ci): auto-version workflow fallback when tag missing

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
- **Circuit Breaker ships disabled by default** (opt-in). It is introduced in this release but stays inactive unless `CB_ENABLED=1` is set, so existing deployments are unaffected. Tunable via `CB_THRESHOLD` (default 10) and `CB_RECOVERY_SECS`.
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
- Stream timeout (120s) + buffer limit (10MB) + graceful shutdown
- SSRF protection (RFC1918/metadata blocklist)
- Context window scaling for CC auto-compact (Kimi 131K→200K)
- StreamTimeout/BufferOverflow error variants
- 10 new tests (42 total)

### Changed
- OnceLock regex cache (~90% faster HTML stripping)
- Unified CalibrationEntry struct (single lock)
- Auto-rebuild binary after version bump in auto-version.sh

### Fixed
- RwLock poison recovery (5 sites)
- AtomicBool reload guard (SIGHUP/watcher race)
- Thinking signature field for Anthropic protocol
- Overflow retry safety margin (NIM re-tokenization death spiral)
- HTTP 408 extracted from Fatal → Retryable

---

## [0.11.0] - 2026-04-18

_No release notes recorded._

---

## [0.10.4] - 2026-04-16

### Changed
- Perf(proxy): fix 3 performance regressions from v10.0-v10.2 update

---

## [0.10.3] - 2026-04-15

### Fixed
- Fix(deps): upgrade rustls-webpki 0.103.10→0.103.12 (RUSTSEC-2026-0098/0099)
- Fix(proxy): auto-clamp max_tokens on input_tokens overflow instead of fatal error

---

## [0.10.2] - 2026-04-15

### Changed
- Comprehensive README rewrite for v0.10.0

### Fixed
- Fix(proxy): sanitize content blocks for </previous_reasoning> XML leakage

---

## [0.10.1] - 2026-04-15

### Fixed
- Fix(cli): pass -c config path to config/setup subcommands + make scan flags mutually exclusive

---

## [0.10.0] - 2026-04-15

### Added
- Setup wizard, config commands, and Option B concurrency refactor
- Feat(config): add config show/set/test subcommands
- Feat(setup): implement interactive setup wizard (6 phases)
- Feat(cli): add Setup and Config subcommands with stub modules
- Feat(config): make MAX_CONCURRENT_PER_MODEL and PERMIT_TIMEOUT_SECS configurable via .env

### Changed
- Add dialoguer, console, indicatif deps for setup wizard
- Add CLAUDE.md to .gitignore

### Fixed
- Fix(config): change default PORT from 3000 to 8315

---

## [0.9.0] - 2026-04-14

### Added
- Feat(installer): auto-configure claude --effort max wrapper in bashrc
- Feat(logging): capture CC thinking/effort params for forensic analysis

### Changed
- Update Cargo.lock from release build
- Ci(hooks): auto-pull after push to sync GitHub Actions version bumps

### Fixed
- Fix(installer): cleanup claude wrapper from bashrc on uninstall

---

## [0.8.0] - 2026-04-13

### Added
- Feat(calibration): dynamic per-model token calibration + auto-deploy via systemd [v8.0]

---

## [0.7.0] - 2026-04-13

### Added
- Feat(tokenizer): inject tiktoken-estimated input_tokens in message_start [v7.0]

---

## [0.6.2] - 2026-04-13

### Changed
- Add CLAUDE.md project context and update Cargo.lock

### Fixed
- Fix(proxy): merge reasoning sanitization and auto-deploy
- Sanitize NIM reasoning_content to prevent XML tool call leakage

---

## [0.6.1] - 2026-04-12

### Changed
- Rewrite README with complete LOC-by-LOC architecture documentation
- Match release notes format to v0.5.0 Keep a Changelog style
- Upgrade to Node.js 24 actions and professional release notes

### Fixed
- Remove legacy nexus-brain references and fix tracing filter module name

---

## [0.6.0] - 2026-04-12

### Added
- Auto-versioning system based on conventional commits (`feat→minor`, `fix→patch`)
- Auto Version & Release GitHub Actions workflow for automated releases
- Portable git hooks in `scripts/hooks/` with `core.hooksPath` configuration
- Post-commit hook showing pending version bump preview
- Pre-push validation hook with version sync, tests, and security audit
- Version sync integration tests (`tests/version_sync.rs`)
- Setup automation (`task setup`, `task setup-hooks`)
- systemd user service installer with log rotation
- Taskfile commands: `auto-version`, `version-check`, `full-release`

### Changed
- Upgraded GitHub Actions from `actions-rs/toolchain@v1` to `dtolnay/rust-toolchain@stable`
- Upgraded `actions/cache` from v3 to v4
- Enhanced `bump-version.sh` with CHANGELOG integration and 3-way verification
- Consolidated cache paths in CI pipeline

### Fixed
- Re-applied deferred `message_delta` (Fix 4) lost in previous commit
- Resolved clippy errors and stream cutoff bug
- Fixed CI cache key typo (`argo-index` → `cargo-index`)

### Security
- Updated dependencies to fix security vulnerabilities
- Added `cargo audit` to pre-push hook validation
- Non-blocking security scan in CI pipeline

---

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

---
