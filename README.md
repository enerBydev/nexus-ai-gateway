# NEXUS-AI-Gateway

**Neuronal EXperience Unified System** — A high-performance API gateway that enables [Claude Code](https://docs.anthropic.com/en/docs/claude-code) to work with any OpenAI-compatible model provider.

> NEXUS-AI-Gateway sits between Claude Code and upstream model providers (NVIDIA NIM, OpenRouter, Ollama, etc.), translating Anthropic API requests into OpenAI-compatible format in real-time — including full SSE streaming, tool calling, thinking/reasoning blocks, and vision.

---

## Table of Contents

- [How It Works](#how-it-works)
- [Features](#features)
- [Installation](#installation)
- [Configuration](#configuration)
- [Usage](#usage)
- [Architecture](#architecture)
- [CLI Reference](#cli-reference)
- [Development](#development)
- [Version History](#version-history)
- [License](#license)

---

## How It Works

Claude Code communicates exclusively via the Anthropic Messages API (`/v1/messages`). NEXUS-AI-Gateway intercepts these requests and performs a bidirectional protocol translation:

```
┌─────────────┐           ┌───────────────────┐           ┌──────────────────┐
│             │  Anthropic │                   │  OpenAI   │                  │
│ Claude Code │  Messages  │  NEXUS-AI-Gateway │  Chat     │  NVIDIA NIM      │
│             │ ─────────▶ │                   │ ────────▶ │  OpenRouter      │
│   (Client)  │           │  • Transform req  │           │  Ollama          │
│             │ ◀───────── │  • Smart retry    │ ◀──────── │  Any OpenAI-     │
│             │  Anthropic │  • Rate limit     │  OpenAI   │  compatible API  │
│             │  Format    │  • Stream convert │  Format   │                  │
└─────────────┘           └───────────────────┘           └──────────────────┘
```

**What gets translated:**

| Anthropic Format | OpenAI Format |
|:-----------------|:--------------|
| `POST /v1/messages` | `POST /v1/chat/completions` |
| `system` (string or block array) | `messages[0].role = "system"` |
| `content_block` with `type: "thinking"` | `reasoning_content` / `reasoning` field |
| `content_block` with `type: "tool_use"` | `tool_calls[]` array |
| `content_block` with `type: "tool_result"` | `messages[].role = "tool"` |
| `stop_reason: "end_turn"` | `finish_reason: "stop"` |
| `stop_reason: "tool_use"` | `finish_reason: "tool_calls"` |
| SSE: `message_start`, `content_block_delta`, `message_stop` | SSE: `data: {...}` chunks with `choices[0].delta` |

---

## Features

### Protocol Translation

- **Anthropic → OpenAI request conversion** — Messages, system prompts, tools, images, thinking blocks
- **OpenAI → Anthropic response conversion** — Including streaming SSE event mapping
- **Model aliasing** — Claude Code sends `claude-sonnet-4-6`, the gateway routes it to `qwen/qwen3-coder-480b-a35b-instruct` (or any NIM model)
- **Thinking/reasoning preservation** — Maps `reasoning_content` from NIM models back to Anthropic `thinking` blocks

### Smart Retry System (3-Layer Error Classification)

Every upstream error is classified through three layers before deciding the action:

| Layer | What It Checks | Action |
|:------|:---------------|:-------|
| **L0: Structural** | NIM's typed error fields (`error.type`, `error.param`) | Immediate classification — e.g., `BadRequestError` + `param=input_tokens` → fatal |
| **L1: Content-Aware** | Pattern matching on error message body | Matches patterns like `"max_tokens"`, `"context_length"` → auto-fix; `"temporarily unavailable"` → retry |
| **L2: Status-Based** | HTTP status code | `429` → retry with 10s base; `502/503` → retry with 3-5s; `400/401/403` → fatal |

**Auto-fix behavior**: When a `max_tokens` overflow is detected, the gateway automatically halves `max_tokens` (minimum 4096) and retries — no user intervention needed.

**Backoff**: Exponential with ±25% jitter, capped at 30 seconds. This prevents thundering herd effects when multiple agents hit the same model simultaneously.

### Concurrency Shield

Per-model semaphore system that prevents overwhelming upstream providers:

- **5 concurrent slots per model** — Empirically matched to NIM's per-model capacity
- **180-second permit timeout** — Returns `529 Overloaded` to Claude Code if the queue is full
- **Streaming-aware** — The semaphore permit lives inside the SSE stream and is only released when the stream completes or disconnects

### Auto-Discovery (Dynamic Context Limits)

On the first request to any model, the gateway probes the upstream for its real `max_total_tokens` limit:

1. Sends a request with `max_tokens=999999`
2. Parses the error message for `max_total_tokens=N`
3. Caches the result for 1 hour
4. Pre-clamps all future requests: `estimated_input_tokens + max_tokens ≤ N`

This prevents token overflow errors before they happen, instead of relying on error-and-retry.

### Multi-Upstream Routing

Route different Claude model aliases to different providers simultaneously:

```bash
# Default upstream
UPSTREAM_BASE_URL=https://integrate.api.nvidia.com
UPSTREAM_API_KEY=nvapi-xxx

# Additional upstreams
UPSTREAM_BIGMODEL_BASE_URL=https://open.bigmodel.cn/api/paas
UPSTREAM_BIGMODEL_API_KEY=xxx

# Model routing table
MODEL_MAP_claude_opus_4_6=default:z-ai/glm5          # Opus → NIM GLM5
MODEL_MAP_claude_sonnet_4_6=bigmodel:glm-4-plus       # Sonnet → BigModel
MODEL_MAP_claude_haiku_4_5=default:moonshotai/kimi-k2.5  # Haiku → NIM Kimi
```

### WebFetch Interceptor

When an upstream model responds with a `web_fetch` tool call, the gateway intercepts it, executes the HTTP GET locally, strips HTML to clean text, and returns the content as a `tool_result` — all transparently to Claude Code.

- Works in both streaming and non-streaming modes
- HTML → text conversion with script/style/nav removal
- Content truncation at 200,000 characters
- Configurable timeout and retry limits

### Hot-Reload

Configuration can be reloaded without restarting the server:

- **SIGHUP signal**: `kill -SIGHUP $(cat /tmp/nexus-ai-gateway.pid)`
- **Automatic file watcher**: Monitors `~/.nexus-ai-gateway.env` for changes and reloads within 1 second

### Claude Code Binary Scanner

Built-in scanner that analyzes the Claude Code binary to discover all supported model IDs, tools, capabilities, and environment variables:

```bash
# Full scan
nexus-ai-gateway scan

# Generate .env template with all discovered model mappings
nexus-ai-gateway scan --env

# Generate launcher script with CC environment variables
nexus-ai-gateway scan --launcher

# Check if CC binary was updated since last scan
nexus-ai-gateway scan --check
```

### Token Counting

Endpoint `/v1/messages/count_tokens` uses the `tiktoken` cl100k_base tokenizer (GPT-4 family) for ~95% accurate token estimation, with per-message overhead accounting.

---

## Installation

### From Source

```bash
git clone https://github.com/enerBydev/nexus-ai-gateway.git
cd nexus-ai-gateway
cargo build --release
```

The binary will be at `target/release/nexus-ai-gateway` (6.9 MB, stripped).

### Install System-Wide

```bash
cargo install --path .
```

### As a systemd Service

```bash
# Interactive installer — creates systemd user service
./scripts/install-service.sh

# Then manage with:
systemctl --user start nexus-ai-gateway
systemctl --user status nexus-ai-gateway
journalctl --user -u nexus-ai-gateway -f
```

### With Task Runner

```bash
# Install task runner first: https://taskfile.dev
task install          # Build + install binary
task setup            # Full setup (install + hooks + service)
```

---

## Configuration

Create `~/.nexus-ai-gateway.env`:

```bash
# ═══════════════════════════════════════════════════
# NEXUS-AI-Gateway Configuration
# ═══════════════════════════════════════════════════

# Server
PORT=8315

# Default upstream (NVIDIA NIM)
UPSTREAM_BASE_URL=https://integrate.api.nvidia.com
UPSTREAM_API_KEY=nvapi-your-key-here

# Model Mapping Table
# Format: MODEL_MAP_<claude_id_with_underscores>=<upstream>:<target_model>
# Note: hyphens in Claude model IDs become underscores in env var names
MODEL_MAP_claude_opus_4_6=default:z-ai/glm5
MODEL_MAP_claude_sonnet_4_6=default:qwen/qwen3-coder-480b-a35b-instruct
MODEL_MAP_claude_haiku_4_5=default:moonshotai/kimi-k2.5

# Model overrides (fallback if no MODEL_MAP match)
# REASONING_MODEL=nvidia/llama-3.3-70b-instruct
# COMPLETION_MODEL=nvidia/llama-3.3-70b-instruct

# WebFetch Interceptor
WEB_FETCH_ENABLED=true
WEB_FETCH_MAX_RETRIES=3
WEB_FETCH_TIMEOUT_SECS=15

# Debug (set to true for detailed request/response logging)
# DEBUG=false
# VERBOSE=false
```

### Config File Search Order

1. `--config /path/to/file` (CLI flag)
2. `./.env` (current directory)
3. `~/.nexus-ai-gateway.env` (home directory)
4. `/etc/nexus-ai-gateway/.env` (system-wide)

### Port Convention

The default port **8315** is derived from the project acronym:

```
N(78) + E(69) + U(85) + S(83) = 315  →  Port 8315
```

### Claude Code Setup

Point Claude Code to the proxy by setting these environment variables before launching:

```bash
export ANTHROPIC_BASE_URL="http://localhost:8315"
export ANTHROPIC_API_KEY="proxy-key"  # Any non-empty value
```

Or generate a complete launcher script:

```bash
nexus-ai-gateway scan --launcher > launch-claude.sh
chmod +x launch-claude.sh
./launch-claude.sh
```

---

## Usage

### Start the Server

```bash
# Foreground mode (for development)
nexus-ai-gateway

# Daemon mode (background)
nexus-ai-gateway --daemon

# With custom config and debug logging
nexus-ai-gateway --config /path/to/.env --debug --port 9000
```

### Manage the Daemon

```bash
nexus-ai-gateway status     # Check if running
nexus-ai-gateway stop       # Stop gracefully
```

### API Endpoints

| Endpoint | Method | Description |
|:---------|:-------|:------------|
| `/v1/messages` | POST | Anthropic Messages API (main proxy endpoint) |
| `/v1/messages/count_tokens` | POST | Token count estimation using tiktoken |
| `/health` | GET | Health check (returns `OK`) |

### Test with curl

```bash
curl -s http://localhost:8315/v1/messages \
  -H "Content-Type: application/json" \
  -H "x-api-key: proxy-key" \
  -H "anthropic-version: 2023-06-01" \
  -d '{
    "model": "claude-sonnet-4-6",
    "max_tokens": 1024,
    "messages": [
      {"role": "user", "content": "What is 2+2?"}
    ]
  }'
```

---

## Architecture

### Source Modules (4,222 LOC)

| Module | LOC | Purpose |
|:-------|----:|:--------|
| `proxy.rs` | 1,426 | Core proxy logic: retry system, concurrency shield, auto-discovery, streaming SSE translation |
| `scan.rs` | 589 | CC binary scanner: model ID extraction, tool discovery, .env/launcher generation |
| `main.rs` | 522 | Entry point: CLI dispatch, server setup, hot-reload watcher, token counting |
| `transform.rs` | 398 | Bidirectional Anthropic ↔ OpenAI protocol conversion |
| `web_fetch.rs` | 262 | WebFetch tool interceptor: HTTP GET, HTML stripping, content truncation |
| `config.rs` | 259 | Configuration loading, multi-upstream, model routing, hot-reload |
| `models/anthropic.rs` | 247 | Anthropic API data types (request/response/streaming) |
| `models/openai.rs` | 189 | OpenAI API data types (request/response/streaming) |
| `watcher.rs` | 135 | CC binary change detection with SHA256 comparison |
| `error.rs` | 95 | Error types with Anthropic-native response formatting |
| `cli.rs` | 68 | CLI argument parsing with clap |

### Key Design Decisions

- **No Claude API key required** — The gateway does not validate Anthropic API keys. Claude Code sends any non-empty value.
- **Thinking forced globally** — All requests enable `enable_thinking=true` via `chat_template_kwargs`. This produces better output from NIM models regardless of Claude Code's effort setting.
- **Model identity preserved** — Responses always return the original Claude model ID (e.g., `claude-sonnet-4-6`), even though the actual model was different. This prevents Claude Code from rejecting responses.
- **Anthropic-native errors** — All error responses use Anthropic's error format (`{"type": "error", "error": {"type": "...", "message": "..."}}`). This ensures Claude Code handles errors correctly (e.g., retrying on `rate_limit_error`, stopping on `invalid_request_error`).

### Dependencies

| Crate | Purpose |
|:------|:--------|
| `axum` | HTTP server framework |
| `reqwest` | HTTP client (with rustls-tls) |
| `tokio` | Async runtime |
| `serde` / `serde_json` | Serialization |
| `tiktoken-rs` | Token counting (cl100k_base) |
| `clap` | CLI argument parsing |
| `tracing` | Structured logging |
| `daemonize` | Background process support |
| `notify` | File system watching for hot-reload |
| `sha2` | CC binary integrity verification |

---

## CLI Reference

```
nexus-ai-gateway [OPTIONS] [COMMAND]

Commands:
  scan     Scan Claude Code binary for model IDs, tools, and capabilities
  stop     Stop running daemon
  status   Check daemon status

Options:
  -c, --config <FILE>   Path to custom .env configuration file
  -d, --debug           Enable debug logging
  -v, --verbose         Enable verbose logging (full request/response bodies)
  -p, --port <PORT>     Port to listen on (overrides PORT env var)
      --daemon          Run as background daemon
      --pid-file <FILE> PID file path [default: /tmp/nexus-ai-gateway.pid]
  -h, --help            Print help
  -V, --version         Print version

Scan subcommand:
  nexus-ai-gateway scan [OPTIONS]
    --env        Generate .env template with model mapping entries
    --launcher   Generate launcher script with CC environment variables
    --check      Check if CC binary was updated since last scan
```

---

## Development

### Prerequisites

- Rust 1.75+ (stable)
- [Task](https://taskfile.dev) (optional, for task automation)

### Setup

```bash
git clone https://github.com/enerBydev/nexus-ai-gateway.git
cd nexus-ai-gateway
task setup   # Installs hooks + builds
```

### Git Hooks

Portable hooks are stored in `scripts/hooks/` and activated via `core.hooksPath`:

| Hook | What It Does |
|:-----|:-------------|
| `pre-commit` | Runs `cargo fmt --check` + `cargo clippy` + secrets scan |
| `commit-msg` | Validates conventional commit format (`feat:`, `fix:`, etc.) |
| `post-commit` | Shows pending version bump preview |
| `pre-push` | 3-way version sync + `cargo test` + `cargo audit` |

### CI/CD Pipeline

Two GitHub Actions workflows:

1. **CI/CD Pipeline** (`ci.yml`) — Runs on every push/PR: lint, format, test, build, security audit
2. **Auto Version & Release** (`auto-version.yml`) — Triggers after CI passes on `main`: analyzes commits, bumps version, creates tag and GitHub Release with binary

Version bumping follows conventional commits:
- `feat:` → MINOR bump (0.5.0 → 0.6.0)
- `fix:` → PATCH bump (0.6.0 → 0.6.1)
- `chore:` / `ci:` / `docs:` → no bump

### Running Tests

```bash
cargo test              # All tests
task test               # Via task runner
task version-check      # Verify VERSION/Cargo.toml/lib.rs sync
```

---

## Version History

| Version | Date | Description |
|:--------|:-----|:------------|
| 0.6.0 | 2026-04-12 | Auto-versioning system, CI/CD hardening, Node.js 24 Actions upgrade |
| 0.5.0 | 2026-04-10 | Smart retry (3-layer), concurrency shield, auto-discovery, multi-upstream |
| 0.1.0 | 2026-03-15 | Initial release: basic proxy with streaming |

---

## License

[MIT License](LICENSE) — © 2026 enerBydev
