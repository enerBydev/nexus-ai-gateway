# CLAUDE.md
This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

NEXUS-AI-Gateway is a high-performance API proxy that translates Anthropic Messages API requests to OpenAI-compatible format. It enables Claude Code to work with any OpenAI-compatible model provider (NVIDIA NIM, OpenRouter, Ollama, etc.).

## Development Commands

```bash
# Build
cargo build --release          # Production build (~7.2 MB binary)
cargo build                    # Debug build

# Testing
cargo test                     # Run all 32 tests
cargo test -- --nocapture      # Show println output
cargo test tokenizer           # Run specific test module

# Linting & Formatting
cargo fmt --check              # Check formatting
cargo clippy -- -D warnings    # Lint (warnings as errors)

# Security Audit
cargo audit                    # Check for CVEs in dependencies

# Task Runner (alternative)
task test                      # Run tests
task version-check             # Verify VERSION/Cargo.toml/lib.rs sync
task setup                     # Full setup (build + hooks)
```

## Architecture

### Core Request Flow
```
Claude Code → POST /v1/messages → Proxy → Transform to OpenAI format → Upstream API
                    ↑                                              ↓
              SSE Stream ← Transform back to Anthropic ← SSE Stream
```

### Key Modules (by size/complexity)

| Module | Purpose |
|--------|---------|
| `proxy.rs` | Core proxy: 3-layer retry system, concurrency shield (per-model semaphores), auto-discovery, SSE streaming |
| `setup.rs` | 6-phase interactive setup wizard |
| `scan.rs` | Claude Code binary scanner (model IDs, tools, capabilities) |
| `transform.rs` | Bidirectional Anthropic ↔ OpenAI protocol conversion |
| `config.rs` | Config loading, multi-upstream routing, model mapping |
| `config_cmd.rs` | `config show/set/test` CLI commands |
| `tokenizer.rs` | Token estimation using tiktoken cl100k_base |
| `web_fetch.rs` | Intercepts `web_fetch` tool calls, fetches locally, strips HTML |
| `models/anthropic.rs` | Anthropic API types (request/response/streaming) |
| `models/openai.rs` | OpenAI API types |
| `prompt_cache.rs` | Proxy-side SHA-256 cache for NIM KV_REUSE (TTL+LRU, 7 unit tests) |

### Critical Design Decisions

1. **No Anthropic API key validation** — Any non-empty key is accepted; the gateway validates upstream credentials only.

2. **Thinking forced globally** — All requests include `enable_thinking=true` via `chat_template_kwargs` for better NIM model output.

3. **Model identity preserved** — Responses always return the original Claude model ID (e.g., `claude-sonnet-4-6`) even when routed to different upstream models.

4. **Anthropic-native errors** — All error responses use Anthropic's error format with proper `type` fields for correct Claude Code handling.

5. **Auto-fix on overflow** — `max_tokens` overflow triggers automatic halving (minimum 4096) with retry.

6. **Prompt caching bridge** — `cache_control` markers are extracted and logged; `anthropic-beta` header sent only to Anthropic upstream; honest zero cache tokens for NIM (no fake cache hits).

## Version Management

Three files must stay synchronized:
- `VERSION` — Single line version string
- `Cargo.toml` — `version = "X.Y.Z"`
- `src/lib.rs` — `pub const VERSION: &str = "X.Y.Z"`

Run `task version-check` to verify sync. CI enforces this on every push.

## Model Mapping Configuration

Environment variables route Claude models to upstream providers:

```bash
# Format: MODEL_MAP_<claude_id_with_underscores>=<upstream>:<model>
MODEL_MAP_claude_opus_4_6=default:z-ai/glm5
MODEL_MAP_claude_sonnet_4_6=bigmodel:glm-4-plus
MODEL_MAP_claude_haiku_4_5=default:moonshotai/kimi-k2.5
```

Hyphens in Claude model IDs become underscores in env var names.

## Git Workflow

- **Main branch is protected** — No direct pushes, PRs required
- **Conventional commits** — `feat:`, `fix:`, `chore:`, `docs:`, `refactor:`, `test:`, `ci:`, `perf:`
- **Pre-commit hooks** — Format check, clippy, secrets scan
- **Pre-push hooks** — Tests, clippy, `cargo audit`, version sync check

## Testing Strategy

- Unit tests in `#[cfg(test)]` modules within source files
- Integration tests in `tests/` directory
- Coverage target: 80%+
- Test modules: `tokenizer_test.rs`, `transform_test.rs`, `error_test.rs`

## Configuration Files

Config search order:
1. `-c, --config <FILE>` (CLI flag)
2. `./.env` (current directory)
3. `~/.nexus-ai-gateway.env` (home directory)
4. `/etc/nexus-ai-gateway/.env` (system-wide)

## API Endpoints

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/v1/messages` | POST | Main proxy endpoint (Anthropic Messages API) |
| `/v1/messages/count_tokens` | POST | Token count estimation |
| `/health` | GET | Health check (returns `OK`) |

## Port Convention

Default port **8315**: N(78) + E(69) + U(85) + S(83) = 315 → 8315
