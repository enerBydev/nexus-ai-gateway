# NEXUS-AI-Gateway

**Neuronal EXperience Unified System** — A high-performance API gateway that enables [Claude Code](https://docs.anthropic.com/en/docs/claude-code) to work with any OpenAI-compatible model provider.

> NEXUS-AI-Gateway sits between Claude Code and upstream model providers (NVIDIA NIM, OpenRouter, Ollama, etc.), translating Anthropic API requests into OpenAI-compatible format in real-time — including full SSE streaming, tool calling, thinking/reasoning blocks, and vision.

---

## Table of Contents

- [How It Works](#how-it-works)
- [Features](#features)
- [Quick Start](#quick-start)
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
- **Model aliasing** — Claude Code sends `claude-sonnet-4-6`, the gateway routes it to any NIM model (e.g., `moonshotai/kimi-k2.5`)
- **Thinking/reasoning preservation** — Maps `reasoning_content` from NIM models back to Anthropic `thinking` blocks

### Setup Wizard

Interactive 6-phase setup wizard that configures the entire proxy in under 2 minutes:

```bash
# Full interactive mode — prompts for URL, API key, model selection, server config
nexus-ai-gateway setup

# Quick mode — only prompts for API key, uses intelligent defaults for everything else
nexus-ai-gateway setup --quick
```

**Phases:**

1. **Upstream Connection** — Validates API key and discovers available models
2. **Model Selection** — Maps Claude tiers (Opus/Sonnet/Haiku) to upstream models
3. **Server Configuration** — Port, concurrency limits, timeouts
4. **Claude Code Integration** — Auto-configures `~/.claude/settings.json` and `~/.bashrc` wrapper
5. **Generate Configuration** — Writes `.env` with all settings and model mappings
6. **Install & Verify** — Restarts systemd service and runs health check

### Configuration Management

```bash
# View current configuration (formatted, with masked API keys)
nexus-ai-gateway config show

# Modify a setting (updates .env and advises restart)
nexus-ai-gateway config set PORT 9000

# Test connectivity, CC binary, proxy health, and model mappings
nexus-ai-gateway config test
```

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

- **5 concurrent slots per model** (configurable via `MAX_CONCURRENT_PER_MODEL`)
- **180-second permit timeout** (configurable via `PERMIT_TIMEOUT_SECS`) — Returns `529 Overloaded` to Claude Code if the queue is full
- **Streaming-aware** — The semaphore permit lives inside the SSE stream and is only released when the stream completes or disconnects

### Auto-Discovery (Dynamic Context Limits)

On the first request to any model, the gateway probes the upstream for its real `max_total_tokens` limit:

1. Sends a request with `max_tokens=999999`
2. Parses the error message for `max_total_tokens=N`
3. Caches the result for 1 hour
4. Pre-clamps all future requests: `estimated_input_tokens + max_tokens ≤ N`

This prevents token overflow errors before they happen, instead of relying on error-and-retry.

### Dynamic Token Calibration

Per-model calibration system that maintains a running ratio between estimated and actual token counts:

- Learns from real upstream responses (`usage.prompt_tokens`)
- Applies calibrated ratios to future input token estimates
- Persists calibration data to `~/.nexus-ai-gateway-calibration.json`
- Improves accuracy from ~95% (tiktoken estimate) to 97%+ after calibration

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
# Full scan — displays all discovered data
nexus-ai-gateway scan

# Generate .env template with all discovered model mappings
nexus-ai-gateway scan --env

# Generate launcher script with CC environment variables
nexus-ai-gateway scan --launcher

# Check if CC binary was updated since last scan
nexus-ai-gateway scan --check
```

> **Note**: `--env`, `--launcher`, and `--check` are mutually exclusive flags.

### Token Counting

Endpoint `/v1/messages/count_tokens` uses the `tiktoken` cl100k_base tokenizer (GPT-4 family) for ~95% accurate token estimation, with per-message overhead accounting. Input token estimates are injected into `message_start` streaming events for Claude Code's context window tracking.

---

## Quick Start

```bash
# 1. Clone and build
git clone https://github.com/enerBydev/nexus-ai-gateway.git
cd nexus-ai-gateway
cargo build --release

# 2. Run the setup wizard (auto-configures everything)
target/release/nexus-ai-gateway setup --quick

# 3. Start the proxy
target/release/nexus-ai-gateway

# 4. Launch Claude Code (it's already configured by the wizard)
claude
```

---

## Installation

### From Source

```bash
git clone https://github.com/enerBydev/nexus-ai-gateway.git
cd nexus-ai-gateway
cargo build --release
```

The binary will be at `target/release/nexus-ai-gateway` (~7.2 MB).

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

### Setup Wizard (Recommended)

The easiest way to configure NEXUS-AI-Gateway:

```bash
# Full interactive wizard — walks through all options
nexus-ai-gateway setup

# Quick mode — just provide your API key, intelligent defaults for the rest
nexus-ai-gateway setup --quick
```

The wizard will:
1. Validate your API key against the upstream provider
2. Scan the Claude Code binary for all supported model IDs (52+ models)
3. Auto-map Claude tiers (Opus/Sonnet/Haiku) to optimal upstream models
4. Configure `~/.claude/settings.json` to point Claude Code at the proxy
5. Install a `claude --effort max` wrapper in `~/.bashrc`
6. Generate `~/.nexus-ai-gateway.env` with the full configuration

### Manual Configuration

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

# Concurrency
MAX_CONCURRENT_PER_MODEL=5
PERMIT_TIMEOUT_SECS=180

# Model Mapping Table
# Format: MODEL_MAP_<claude_id_with_underscores>=<upstream>:<target_model>
# Note: hyphens in Claude model IDs become underscores in env var names
MODEL_MAP_claude_opus_4_6=default:z-ai/glm5
MODEL_MAP_claude_sonnet_4_6=default:moonshotai/kimi-k2.5
MODEL_MAP_claude_haiku_4_5=default:moonshotai/kimi-k2.5

# WebFetch Interceptor
WEB_FETCH_ENABLED=true
WEB_FETCH_MAX_RETRIES=3
WEB_FETCH_TIMEOUT_SECS=15

# Debug (set to true for detailed request/response logging)
# DEBUG=false
# VERBOSE=false
```

### Configuration Management

```bash
# View current settings (formatted output, masked secrets)
nexus-ai-gateway config show

# Modify a setting
nexus-ai-gateway config set MAX_CONCURRENT_PER_MODEL 10

# Validate connectivity, CC binary, proxy health, model mappings
nexus-ai-gateway config test
```

Use `-c` to target a custom config file:
```bash
nexus-ai-gateway -c /path/to/custom.env config show
nexus-ai-gateway -c /path/to/custom.env setup --quick
```

### Config File Search Order

1. `-c, --config /path/to/file` (CLI flag)
2. `./.env` (current directory)
3. `~/.nexus-ai-gateway.env` (home directory)
4. `/etc/nexus-ai-gateway/.env` (system-wide)

### Port Convention

The default port **8315** is derived from the project acronym:

```
N(78) + E(69) + U(85) + S(83) = 315  →  Port 8315
```

### Claude Code Setup

The setup wizard configures Claude Code automatically. For manual setup, point Claude Code to the proxy:

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
nexus-ai-gateway -c /path/to/.env -d -p 9000
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

### Source Modules (6,065 LOC)

| Module | LOC | Purpose |
|:-------|----:|:--------|
| `proxy.rs` | 1,524 | Core proxy logic: retry system, concurrency shield, auto-discovery, streaming SSE translation |
| `setup.rs` | 691 | Interactive setup wizard: 6-phase configuration, upstream validation, CC integration |
| `scan.rs` | 589 | CC binary scanner: model ID extraction, tool discovery, .env/launcher generation |
| `main.rs` | 477 | Entry point: CLI dispatch, server setup, hot-reload watcher, token counting |
| `transform.rs` | 444 | Bidirectional Anthropic ↔ OpenAI protocol conversion |
| `config_cmd.rs` | 345 | Configuration commands: show (formatted), set (CRUD), test (connectivity) |
| `tokenizer.rs` | 262 | Token estimation using tiktoken cl100k_base with per-message overhead |
| `config.rs` | 273 | Configuration loading, multi-upstream, model routing, hot-reload |
| `web_fetch.rs` | 262 | WebFetch tool interceptor: HTTP GET, HTML stripping, content truncation |
| `models/anthropic.rs` | 247 | Anthropic API data types (request/response/streaming) |
| `models/openai.rs` | 189 | OpenAI API data types (request/response/streaming) |
| `watcher.rs` | 135 | CC binary change detection with SHA256 comparison |
| `error.rs` | 95 | Error types with Anthropic-native response formatting |
| `cli.rs` | 94 | CLI argument parsing with clap (5 subcommands, 7 global flags) |

**Test modules**: `tokenizer_test.rs` (345 LOC), `transform_test.rs` (61 LOC), `error_test.rs` (22 LOC) — 32 tests total.

### Key Design Decisions

- **No Claude API key required** — The gateway does not validate Anthropic API keys. Claude Code sends any non-empty value.
- **Thinking forced globally** — All requests enable `enable_thinking=true` via `chat_template_kwargs`. This produces better output from NIM models regardless of Claude Code's effort setting.
- **Model identity preserved** — Responses always return the original Claude model ID (e.g., `claude-sonnet-4-6`), even though the actual model was different. This prevents Claude Code from rejecting responses.
- **Anthropic-native errors** — All error responses use Anthropic's error format (`{"type": "error", "error": {"type": "...", "message": "..."}}`). This ensures Claude Code handles errors correctly (e.g., retrying on `rate_limit_error`, stopping on `invalid_request_error`).
- **Mutually exclusive scan flags** — `--env`, `--launcher`, and `--check` cannot be combined, enforced at the CLI level via clap's `conflicts_with`.

### Dependencies

| Crate | Purpose |
|:------|:--------|
| `axum` 0.7 | HTTP server framework (with HTTP/2) |
| `reqwest` 0.12 | HTTP client (rustls-tls, streaming, blocking) |
| `tokio` 1.42 | Async runtime (multi-thread, signals) |
| `serde` / `serde_json` | Serialization |
| `tiktoken-rs` | Token counting (cl100k_base) |
| `clap` 4.5 | CLI argument parsing (derive mode) |
| `tracing` | Structured logging |
| `daemonize` | Background process support |
| `notify` / `notify-debouncer-mini` | File system watching for hot-reload |
| `sha2` | CC binary integrity verification |
| `dialoguer` | Interactive prompts (setup wizard) |
| `console` | Terminal formatting and colors |
| `indicatif` | Progress spinners (setup wizard) |
| `chrono` | Timestamps in generated configs |

---

## CLI Reference

```
nexus-ai-gateway [OPTIONS] [COMMAND]

Commands:
  stop     Stop running daemon
  status   Check daemon status
  scan     Scan Claude Code binary for model IDs, tools, and capabilities
  setup    Interactive setup wizard for initial configuration
  config   View or modify configuration
  help     Print this message or the help of the given subcommand(s)

Global Options:
  -c, --config <FILE>   Path to custom .env configuration file
  -d, --debug           Enable debug logging (same as DEBUG=true)
  -v, --verbose         Enable verbose logging (logs full request/response bodies)
  -p, --port <PORT>     Port to listen on (overrides PORT env var)
      --daemon          Run as background daemon
      --pid-file <FILE> PID file path [default: /tmp/nexus-ai-gateway.pid]
  -h, --help            Print help
  -V, --version         Print version
```

### `scan` subcommand

```bash
nexus-ai-gateway scan              # Full scan — display all discovered data
nexus-ai-gateway scan --env        # Generate .env template with MODEL_MAP entries
nexus-ai-gateway scan --launcher   # Generate launcher script with CC env vars
nexus-ai-gateway scan --check      # Check if CC binary changed since last scan
```

> Flags `--env`, `--launcher`, `--check` are mutually exclusive.

### `setup` subcommand

```bash
nexus-ai-gateway setup             # Full interactive wizard (6 phases)
nexus-ai-gateway setup --quick     # Quick mode — only API key, rest auto-selected
nexus-ai-gateway -c out.env setup  # Write config to custom file instead of ~/.nexus-ai-gateway.env
```

### `config` subcommand

```bash
nexus-ai-gateway config show                   # View current config (formatted)
nexus-ai-gateway config set KEY VALUE          # Set a value in .env
nexus-ai-gateway config test                   # Validate connectivity + models
nexus-ai-gateway -c /path.env config show      # Show config from custom file
```

### `stop` / `status` subcommands

```bash
nexus-ai-gateway status                        # Check if daemon is running
nexus-ai-gateway stop                          # Stop the daemon
nexus-ai-gateway stop --pid-file /custom.pid   # Stop with custom PID file
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
| `pre-commit` | Runs `cargo fmt --check` + `cargo clippy -D warnings` + secrets scan |
| `commit-msg` | Validates conventional commit format (`feat:`, `fix:`, etc.) |
| `post-commit` | Shows pending version bump preview |
| `pre-push` | 3-way version sync + `cargo test` + `cargo clippy` + `cargo audit` |

### CI/CD Pipeline

Two GitHub Actions workflows:

1. **CI/CD Pipeline** (`ci.yml`) — Runs on every push/PR: lint, format, test, build, security audit
2. **Auto Version & Release** (`auto-version.yml`) — Triggers after CI passes on `main`: analyzes commits, bumps version, creates tag and GitHub Release with binary

Version bumping follows conventional commits:
- `feat:` → MINOR bump (0.9.0 → 0.10.0)
- `fix:` → PATCH bump (0.10.0 → 0.10.1)
- `chore:` / `ci:` / `docs:` → no bump

### Running Tests

```bash
cargo test              # All 32 tests
cargo clippy -- -D warnings  # Lint check
task test               # Via task runner
task version-check      # Verify VERSION/Cargo.toml/lib.rs sync
```

---

## Version History

| Version | Date | Description |
|:--------|:-----|:------------|
| 0.10.0 | 2026-04-14 | Setup wizard (6 phases), config show/set/test commands, `-c` flag support for all subcommands, scan flags mutually exclusive |
| 0.9.0 | 2026-04-13 | Claude `--effort max` bashrc wrapper, CC thinking/effort forensic logging, installer cleanup |
| 0.8.0 | 2026-04-13 | Dynamic per-model token calibration, auto-deploy via systemd, configurable concurrency |
| 0.7.0 | 2026-04-12 | Tiktoken-based token estimation injected into streaming, cl100k_base tokenizer module |
| 0.6.2 | 2026-04-12 | Reasoning content sanitization, XML tool call leakage prevention |
| 0.6.1 | 2026-04-12 | Remove legacy nexus-brain references, fix tracing filter module name |
| 0.6.0 | 2026-04-12 | Auto-versioning system, CI/CD hardening, Node.js 24 Actions upgrade |
| 0.5.0 | 2026-04-10 | Smart retry (3-layer), concurrency shield, auto-discovery, multi-upstream, WebFetch interceptor |
| 0.1.0 | 2026-03-15 | Initial release: basic proxy with streaming |

---

## License

[MIT License](LICENSE) — © 2026 enerBydev
