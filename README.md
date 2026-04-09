# NEXUS-BRAIN
## Neuronal EXperience Unified System

> AI API Gateway for Claude Code + NVIDIA NIM

---

## What is nexus-ai-gateway?

**nexus-ai-gateway** is an AI API gateway that enables Claude Code to work with
OpenAI-compatible providers like NVIDIA NIM. It automatically translates
between Anthropic and OpenAI API formats, providing:

- ✅ Use Claude Code with alternative model providers
- ✅ Intelligent adaptive rate limiting
- ✅ Automatic retry with exponential backoff
- ✅ Full SSE streaming support
- ✅ Thinking/reasoning modes
- ✅ Tool calling support

---

## Key Features

### 🔄 Smart Retry System (3-Layer)

```
┌─────────────────────────────────────────────────────────────┐
│ Layer 0: Structural Errors                                  │
│ - Parse errors, network failures                            │
│ - Immediate retry                                           │
├─────────────────────────────────────────────────────────────┤
│ Layer 1: Content-Aware Errors                               │
│ - input_tokens overflow                                      │
│ - Auto-truncate and retry                                   │
├─────────────────────────────────────────────────────────────┤
│ Layer 2: Status-Based Errors                                │
│ - HTTP 429 (Rate Limit)                                     │
│ - HTTP 503 (Overloaded)                                     │
│ - Exponential backoff with jitter                           │
└─────────────────────────────────────────────────────────────┘
```

### 🛡️ Concurrency Shield

```rust
MAX_CONCURRENT_PER_MODEL = 5   // Slots per model
PERMIT_TIMEOUT_SECS = 180      // 3 minute timeout

// Per-model semaphores with OwnedSemaphorePermit
// for long-running streaming
```

### 🔍 Auto-Discovery

```rust
// Dynamic probing of max_total_tokens
// Cache TTL: 3600 seconds
// Fallback to safe values
```

### ⚡ Hot-Reload

```bash
# Reload configuration without restart
kill -SIGHUP $(cat /tmp/nexus-ai-gateway.pid)

# Or automatically when .env changes
# (file watcher active)
```

---

## Port Configuration

The default port **8315** is derived from the project name:

```
"Neuronal EXperience Unified System"
 N=78 + E=69 + U=85 + S=83 = 315
 Port = 8000 + 315 = 8315
```

This ensures the port is unique and avoids conflicts with common development ports.

---

## Installation

### From Source

```bash
# Clone
git clone https://github.com/enerBydev/nexus-ai-gateway.git
cd nexus-ai-gateway

# Build release
cargo build --release

# Install
cargo install --path .
```

### With Task

```bash
task install
```

---

## Configuration

### Environment Variables

Create `~/.nexus-ai-gateway.env`:

```bash
# Server Configuration
PORT=8315
HOST=0.0.0.0

# Upstream Configuration
UPSTREAM_BASE_URL=https://integrate.api.nvidia.com/v1
UPSTREAM_API_KEY=${NB_Key}  # Use environment variable

# Model Mappings (Claude ID → NIM Model)
MODEL_MAP_claude_opus_4_6=default:z-ai/glm5
MODEL_MAP_claude_sonnet_4_6=default:qwen/qwen3-coder-480b-a35b-instruct
MODEL_MAP_claude_haiku_4_5=default:moonshotai/kimi-k2.5
```

### API Key Security

Set the API key as an environment variable:

```bash
# Add to ~/.bashrc
export NB_Key="nvapi-xxx..."

# Then reference in config:
UPSTREAM_API_KEY=${NB_Key}
```

---

## Usage

### Start the Server

```bash
# Foreground (debug)
nexus-ai-gateway

# Daemon mode
nexus-ai-gateway --daemon

# With custom config
nexus-ai-gateway --config /path/to/.env
```

### Endpoints

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/v1/chat/completions` | POST | Chat completions (Anthropic format) |
| `/health` | GET | Health check |
| `/count-tokens` | POST | Token counting |

### Example Request

```bash
curl -X POST http://localhost:8315/v1/chat/completions \
  -H "Content-Type: application/json" \
  -H "x-api-key: your-api-key" \
  -H "anthropic-version: 2023-06-01" \
  -d '{
    "model": "claude-sonnet-4-6",
    "max_tokens": 1024,
    "messages": [
      {"role": "user", "content": "Hello!"}
    ]
  }'
```

---

## Architecture

```
Claude Code          nexus-ai-gateway           NVIDIA NIM
     │                    │                     │
     │  Anthropic API     │                     │
     │ ──────────────────▶│                     │
     │                    │  Transform          │
     │                    │  Anthropic → OpenAI │
     │                    │ ───────────────────▶│
     │                    │                     │
     │                    │  OpenAI Response    │
     │                    │ ◀───────────────────│
     │                    │  Transform          │
     │                    │  OpenAI → Anthropic │
     │  Anthropic Format  │                     │
     │ ◀──────────────────│                     │
```

---

## Version History

| Version | Description |
|---------|-------------|
| 0.5.0 | Smart Retry + Concurrency Shield implementation |
| 0.1.0 | Initial development version |

---

## License

MIT License - See [LICENSE](LICENSE)

---

## Credits

Developed as an AI API gateway for Claude Code + NVIDIA NIM integration.
