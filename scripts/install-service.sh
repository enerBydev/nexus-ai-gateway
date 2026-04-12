#!/usr/bin/env bash
# install-service.sh — Install/update nexus-ai-gateway systemd user service
# Part of NEXUS-AI-Gateway project
# Usage: ./scripts/install-service.sh [--uninstall]

set -euo pipefail

SERVICE_NAME="nexus-ai-gateway"
SERVICE_FILE="scripts/${SERVICE_NAME}.service"
USER_SERVICE_DIR="${HOME}/.config/systemd/user"
INSTALLED_SERVICE="${USER_SERVICE_DIR}/${SERVICE_NAME}.service"
BINARY_PATH="${HOME}/.cargo/bin/${SERVICE_NAME}"
ENV_FILE="${HOME}/.nexus-ai-gateway.env"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
CYAN='\033[0;36m'
NC='\033[0m'

info()  { echo -e "${CYAN}ℹ️  $*${NC}"; }
ok()    { echo -e "${GREEN}✅ $*${NC}"; }
warn()  { echo -e "${YELLOW}⚠️  $*${NC}"; }
err()   { echo -e "${RED}❌ $*${NC}"; }

# --- Uninstall ---
if [[ "${1:-}" == "--uninstall" ]]; then
    info "Uninstalling ${SERVICE_NAME} service..."
    systemctl --user stop "${SERVICE_NAME}" 2>/dev/null && ok "Stopped" || true
    systemctl --user disable "${SERVICE_NAME}" 2>/dev/null && ok "Disabled" || true
    rm -f "${INSTALLED_SERVICE}" && ok "Removed service file"
    systemctl --user daemon-reload
    ok "Service uninstalled"
    exit 0
fi

# --- Pre-flight checks ---
echo ""
echo "╔══════════════════════════════════════════════╗"
echo "║  NEXUS-AI-Gateway Service Installer          ║"
echo "╚══════════════════════════════════════════════╝"
echo ""

# 1. Binary exists?
if [[ ! -x "${BINARY_PATH}" ]]; then
    err "Binary not found at ${BINARY_PATH}"
    info "Run 'task install' first to build and install the binary"
    exit 1
fi
ok "Binary found: ${BINARY_PATH}"

# 2. Config exists?
if [[ ! -f "${ENV_FILE}" ]]; then
    warn "Config not found at ${ENV_FILE}"
    info "Generating template with: ${SERVICE_NAME} scan --env > ${ENV_FILE}"
    "${BINARY_PATH}" scan --env > "${ENV_FILE}"
    warn "⚠️  Edit ${ENV_FILE} and set UPSTREAM_API_KEY before starting"
fi
ok "Config: ${ENV_FILE}"

# 3. Check API key is not a shell variable reference
if grep -q 'UPSTREAM_API_KEY=\${' "${ENV_FILE}"; then
    warn "UPSTREAM_API_KEY uses a shell variable (\${...})"
    warn "systemd won't expand shell variables — hardcode the key!"
    echo ""
    read -rp "   Continue anyway? [y/N] " answer
    [[ "${answer}" =~ ^[Yy]$ ]] || exit 1
fi

# 4. Service file exists in project?
if [[ ! -f "${SERVICE_FILE}" ]]; then
    err "Service file not found: ${SERVICE_FILE}"
    info "Run this from the project root directory"
    exit 1
fi
ok "Service template: ${SERVICE_FILE}"

# --- Install ---
info "Installing service..."

# Kill any existing daemon mode process (avoid conflict)
if [[ -f /tmp/nexus-ai-gateway.pid ]]; then
    OLD_PID=$(cat /tmp/nexus-ai-gateway.pid 2>/dev/null)
    if kill -0 "${OLD_PID}" 2>/dev/null; then
        warn "Stopping old daemon (PID ${OLD_PID})..."
        kill "${OLD_PID}" 2>/dev/null || true
        sleep 1
        kill -9 "${OLD_PID}" 2>/dev/null || true
        ok "Old daemon stopped"
    fi
    rm -f /tmp/nexus-ai-gateway.pid
fi

# Create systemd user dir
mkdir -p "${USER_SERVICE_DIR}"

# Copy service file
cp "${SERVICE_FILE}" "${INSTALLED_SERVICE}"
ok "Installed to ${INSTALLED_SERVICE}"

# Reload systemd
systemctl --user daemon-reload
ok "systemd daemon reloaded"

# Enable (auto-start on login)
systemctl --user enable "${SERVICE_NAME}"
ok "Enabled (auto-start on login)"

# Enable linger (start before login, survive logout)
if ! loginctl show-user "$(whoami)" 2>/dev/null | grep -q "Linger=yes"; then
    info "Enabling linger for $(whoami)..."
    loginctl enable-linger "$(whoami)" 2>/dev/null || warn "Could not enable linger (may need sudo)"
fi
ok "Linger: enabled (service survives logout)"

# Start
systemctl --user start "${SERVICE_NAME}"
ok "Started!"

# Verify
sleep 2
if systemctl --user is-active --quiet "${SERVICE_NAME}"; then
    ok "Service is RUNNING"
    echo ""
    echo "─────────────────────────────────────────────"
    systemctl --user status "${SERVICE_NAME}" --no-pager -l 2>/dev/null | head -15
    echo "─────────────────────────────────────────────"
else
    err "Service failed to start!"
    echo ""
    journalctl --user -u "${SERVICE_NAME}" --no-pager -n 20
    exit 1
fi

# Health check
sleep 1
PORT=$(grep "^PORT=" "${ENV_FILE}" 2>/dev/null | cut -d= -f2 || echo "8315")
HEALTH=$(curl -s --max-time 3 "http://localhost:${PORT}/health" 2>/dev/null)
if [[ "${HEALTH}" == "OK" ]]; then
    ok "Health check: OK (port ${PORT})"
else
    warn "Health check failed (port ${PORT} may need a moment)"
fi

echo ""
echo "╔══════════════════════════════════════════════╗"
echo "║  Installation complete!                       ║"
echo "╠══════════════════════════════════════════════╣"
echo "║                                              ║"
echo "║  Control commands:                            ║"
echo "║    systemctl --user status  ${SERVICE_NAME}  ║"
echo "║    systemctl --user restart ${SERVICE_NAME}  ║"
echo "║    systemctl --user stop    ${SERVICE_NAME}  ║"
echo "║    journalctl --user -u ${SERVICE_NAME} -f   ║"
echo "║                                              ║"
echo "║  Config: ~/.nexus-ai-gateway.env              ║"
echo "║  Logs:   /tmp/nexus-ai-gateway.log            ║"
echo "║                                              ║"
echo "║  Reload config (no restart needed):           ║"
echo "║    systemctl --user reload ${SERVICE_NAME}   ║"
echo "║    (or just edit .env — auto-detected)        ║"
echo "║                                              ║"
echo "║  Uninstall:                                   ║"
echo "║    ./scripts/install-service.sh --uninstall   ║"
echo "║                                              ║"
echo "╚══════════════════════════════════════════════╝"
