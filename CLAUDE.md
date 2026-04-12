# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build & Development Commands

```bash
# Build
cargo build --release          # Release binary (6.9 MB, stripped)
cargo build                    # Debug build

# Test
cargo test                     # All tests
cargo test -- --nocapture      # With stdout output
cargo test version             # Run specific test pattern

# Lint & Format
cargo fmt -- --check           # Check formatting
cargo clippy -- -D warnings    # Clippy with warnings as errors
cargo fmt && cargo clippy -- -D warnings  # Fix format + check

# Run
cargo run                      # Development mode
cargo run -- --daemon          # Background daemon
cargo run -- --help            # CLI reference

# Task runner (alternative)
task build                     # Build release
task test                      # Run tests
task check                     # fmt-check + lint + test
task setup                     # Full setup (install + hooks + service)
```

## Architecture Overview

NEXUS-AI-Gateway is an Anthropic API proxy that translates requests to OpenAI-compatible endpoints. Key modules:

| Module | Purpose |
|--------|---------|
| `proxy.rs` | Core proxy logic: retry system (3-layer error classification), concurrency shield (per-model semaphores), auto-discovery (dynamic context limits), streaming SSE translation |
| `transform.rs` | Bidirectional Anthropic ↔ OpenAI protocol conversion (request/response/streaming) |
| `config.rs` | Configuration loading, multi-upstream routing, model mapping, hot-reload |
| `scan.rs` | Claude Code binary scanner: model ID extraction, tool discovery, .env/launcher generation |
| `web_fetch.rs` | WebFetch tool interceptor: HTTP GET, HTML→text conversion, content truncation |
| `error.rs` | Error types with Anthropic-native response formatting |
| `models/anthropic.rs` | Anthropic API types (request/response/streaming) |
| `models/openai.rs` | OpenAI API types (request/response/streaming) |

### Key Design Patterns

1. **3-Layer Error Classification** (proxy.rs): L0 structural (typed error fields) → L1 content-aware (pattern matching) → L2 status-based. Determines retry vs fatal.

2. **Concurrency Shield**: Per-model semaphores (5 slots, 180s timeout) prevent upstream overload. Semaphore permit lives inside SSE stream.

3. **Auto-Discovery**: First request to a model probes for `max_total_tokens`, caches for 1 hour, pre-clamps all future requests.

4. **Streaming-First**: SSE translation happens in-stream. No buffering of complete responses.

### Data Flow

```
Claude Code → /v1/messages → [proxy.rs] → transform.rs (Anthropic→OpenAI)
                                                    ↓
                                              upstream request
                                                    ↓
                                              [retry/semaphore]
                                                    ↓
                                              upstream response
                                                    ↓
                                            transform.rs (OpenAI→Anthropic)
                                                    ↓
Claude Code ← SSE stream ← [proxy.rs] ←────────────┘
```

## Version Management

Three sources must stay synchronized:
- `VERSION` file (source of truth)
- `Cargo.toml` `version = "..."`
- `src/lib.rs` `pub const VERSION: &str = "..."`

```bash
# Verify sync
task version-check

# Bump version (updates all 3 sources)
./scripts/bump-version.sh X.Y.Z

# Auto-version from conventional commits
task auto-version-dry    # Preview
task auto-version        # Apply
```

## Git Hooks

Portable hooks in `scripts/hooks/` activated via `git config core.hooksPath`:

| Hook | Actions |
|------|---------|
| `pre-commit` | `cargo fmt --check` + `cargo clippy` + secrets scan |
| `commit-msg` | Conventional commit format validation (`feat:`, `fix:`, etc.) |
| `post-commit` | Version bump preview |
| `pre-push` | 3-way version sync + `cargo test` + `cargo audit` |

```bash
task setup-hooks  # Activate hooks
```

## Configuration

Config file search order:
1. `--config /path/to/file` (CLI flag)
2. `./.env` (current directory)
3. `~/.nexus-ai-gateway.env` (home directory)
4. `/etc/nexus-ai-gateway/.env` (system-wide)

Model routing via env vars: `MODEL_MAP_claude_sonnet_4_6=default:qwen/qwen3-coder-480b`

Hot-reload: SIGHUP or file watcher on config file.

## Testing Notes

- Integration tests in `tests/` run as separate binaries
- `version_sync.rs` verifies VERSION/Cargo.toml/lib.rs sync
- No external services required for tests
- For proxy testing, mock upstream responses (no real API calls)

## Common Development Tasks

When adding new features:
1. Update models in `models/anthropic.rs` or `models/openai.rs` if changing API types
2. Update `transform.rs` if changing protocol conversion logic
3. Update `proxy.rs` if changing retry/semaphore/streaming behavior
4. Run `task check` before committing

When fixing bugs:
1. Check error classification in `proxy.rs` (L0/L1/L2 layers)
2. Check streaming SSE conversion in `transform.rs`
3. Add regression test in appropriate module
