# Security Policy

## Reporting a Vulnerability
Please report security vulnerabilities by emailing the maintainer directly.
Do NOT file public issues for security vulnerabilities.

## Supported Versions

| Version | Supported |
| ------- | --------- |
| 0.13.x  | ✅ Active |
| < 0.13  | ❌         |

## Security Features
- API keys stored in .env files (never hardcoded)
- **Loopback-only bind by default** (`BIND_ADDR=127.0.0.1`) — not reachable from the network unless opted in
- **Optional IP allowlist** (`ALLOWED_IPS`) — per-request access control, defense-in-depth
- SSRF protection in WebFetch (RFC1918 IP blocking)
- CORS defaults to localhost only
- Circuit breaker for cascade failure prevention
- .env file permissions set to 600 on Unix
- systemd `RestrictAddressFamilies` network hardening

## Network Exposure & Access Control (Issue #78)

By default NEXUS binds to **`127.0.0.1`** and is reachable only from the local
machine — exactly what the Claude Code client needs (`ANTHROPIC_BASE_URL=http://localhost:<port>`).
Exposing the proxy to other devices is an explicit, layered opt-in:

| Layer | Variable / artifact | Default | Purpose |
|-------|---------------------|---------|---------|
| Bind address | `BIND_ADDR` (or `--bind`) | `127.0.0.1` | Which interfaces the listener accepts. `0.0.0.0` = all interfaces (opt-in). Emits a warning when non-loopback. |
| IP allowlist | `ALLOWED_IPS` | empty (allow all) | Comma-separated CIDRs/IPs permitted to reach the proxy. Loopback is **always** allowed. |
| Host firewall | `scripts/harden-firewall.sh` | not applied | Explicit UFW rule for the port (`--dry-run`, `--allow-lan <cidr>`). |
| systemd | `scripts/nexus-ai-gateway.service` | `RestrictAddressFamilies` | Restricts socket families. (IP egress filtering intentionally NOT enabled — it would break outbound calls to the upstream.) |

**Legacy `HOST` variable:** deprecated and ignored — it never controlled the
listener. Use `BIND_ADDR` instead. A warning is logged if `HOST` is set without `BIND_ADDR`.

**To expose on a LAN safely:**
```bash
# 1) Bind to all interfaces (opt-in)
BIND_ADDR=0.0.0.0
# 2) Restrict who may connect
ALLOWED_IPS=192.168.1.0/24
# 3) Make the firewall rule explicit
./scripts/harden-firewall.sh --allow-lan 192.168.1.0/24
```

> Changing `BIND_ADDR` or `ALLOWED_IPS` requires a restart (the socket and middleware
> are configured once at startup; they are not hot-reloaded).
