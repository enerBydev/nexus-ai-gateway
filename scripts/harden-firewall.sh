#!/usr/bin/env bash
# ============================================================
# harden-firewall.sh — Explicit UFW rule for the NEXUS port (Issue #78, Solution D).
#
# By default NEXUS binds to 127.0.0.1 (Solution A), so the port is already
# unreachable from the network. This script makes that protection EXPLICIT at the
# host firewall, so it survives even if someone later sets BIND_ADDR=0.0.0.0 or
# disables the implicit "default deny incoming" policy.
#
# Usage:
#   ./scripts/harden-firewall.sh                         # deny <PORT>/tcp (default 8315)
#   PORT=8316 ./scripts/harden-firewall.sh               # custom port via env
#   ./scripts/harden-firewall.sh --port 8316             # custom port via flag
#   ./scripts/harden-firewall.sh --allow-lan 192.168.1.0/24   # allow ONLY that CIDR
#   ./scripts/harden-firewall.sh --dry-run               # print commands, change nothing
#
# Idempotent: UFW skips rules that already exist. Requires `ufw` + sudo.
# ============================================================
set -euo pipefail

PORT="${PORT:-8315}"
DRY_RUN=0
ALLOW_LAN=""

print_help() {
    sed -n '2,18p' "$0" | sed 's/^# \{0,1\}//'
}

while [ $# -gt 0 ]; do
    case "$1" in
        --dry-run) DRY_RUN=1; shift ;;
        --allow-lan)
            ALLOW_LAN="${2:-}"
            [ -n "$ALLOW_LAN" ] || { echo "❌ --allow-lan requires a CIDR (e.g. 192.168.1.0/24)" >&2; exit 2; }
            shift 2 ;;
        --port)
            PORT="${2:-}"
            shift 2 ;;
        -h|--help) print_help; exit 0 ;;
        *) echo "❌ Unknown argument: $1" >&2; exit 2 ;;
    esac
done

# Validate port range.
if ! [[ "$PORT" =~ ^[0-9]+$ ]] || [ "$PORT" -lt 1 ] || [ "$PORT" -gt 65535 ]; then
    echo "❌ Invalid PORT: '$PORT' (expected 1-65535)" >&2
    exit 2
fi

if ! command -v ufw >/dev/null 2>&1; then
    echo "❌ 'ufw' not found. Install it (sudo apt install ufw) or configure your firewall manually." >&2
    exit 1
fi

# Build the rule as an array (safe quoting).
if [ -n "$ALLOW_LAN" ]; then
    RULE=(ufw allow from "$ALLOW_LAN" to any port "$PORT" proto tcp)
    echo "🔓 Allow ONLY $ALLOW_LAN to reach port $PORT/tcp; all other sources fall through to 'default deny incoming'."
else
    RULE=(ufw deny "$PORT"/tcp)
    echo "🔒 Explicitly DENY port $PORT/tcp from all sources (matches the secure default BIND_ADDR=127.0.0.1)."
fi

if [ "$DRY_RUN" -eq 1 ]; then
    echo "🧪 DRY-RUN — no changes made. Would execute:"
    if [ -n "$ALLOW_LAN" ]; then
        echo "   sudo ufw delete deny $PORT/tcp   # remove any conflicting blanket-deny first"
    fi
    echo "   sudo ${RULE[*]}"
    exit 0
fi

# In allow-lan mode, drop a prior blanket deny so the allow rule can take effect
# (UFW evaluates rules in order; a leading deny would shadow the allow).
if [ -n "$ALLOW_LAN" ]; then
    sudo ufw delete deny "$PORT"/tcp 2>/dev/null || true
fi

echo "▶  sudo ${RULE[*]}"
sudo "${RULE[@]}"

echo "✅ Done. Current UFW rules mentioning port $PORT:"
sudo ufw status | grep -E "(^| )$PORT(/| |\b)" || echo "   (none shown — run 'sudo ufw status numbered' to inspect)"
