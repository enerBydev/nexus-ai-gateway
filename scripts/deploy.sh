#!/usr/bin/env bash
# deploy.sh — Automated pull+build+install+restart for nexus-ai-gateway
# Part of NEXUS-AI-Gateway project
# Usage: ./scripts/deploy.sh [--branch <name>] [--force] [--no-restart] [--check] [--help]

set -euo pipefail

SERVICE_NAME="nexus-ai-gateway"
BINARY_NAME="nexus-ai-gateway"
ENV_FILE="${HOME}/.nexus-ai-gateway.env"
USER_SERVICE_DIR="${HOME}/.config/systemd/user"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
CYAN='\033[0;36m'
NC='\033[0m'

info() { echo -e "${CYAN}ℹ️ $*${NC}"; }
ok() { echo -e "${GREEN}✅ $*${NC}"; }
warn() { echo -e "${YELLOW}⚠️ $*${NC}"; }
err() { echo -e "${RED}❌ $*${NC}"; }

# Default values
BRANCH="main"
FORCE=false
NO_RESTART=false
CHECK_ONLY=false

# Parse arguments
while [[ $# -gt 0 ]]; do
    case "$1" in
        --branch)
            BRANCH="${2:-}"
            if [[ -z "${BRANCH}" ]]; then
                err "--branch requires a value"
                exit 1
            fi
            shift 2
            ;;
        --force)
            FORCE=true
            shift
            ;;
        --no-restart)
            NO_RESTART=true
            shift
            ;;
        --check)
            CHECK_ONLY=true
            shift
            ;;
        --help)
            echo "Usage: $0 [--branch <name>] [--force] [--no-restart] [--check] [--help]"
            echo ""
            echo "Options:"
            echo "  --branch <name>  Deploy from a different branch (default: main)"
            echo "  --force          Skip uncommitted changes check"
            echo "  --no-restart     Build and install only, don't restart service"
            echo "  --check          Just check current status without deploying"
            echo "  --help           Show this help message"
            exit 0
            ;;
        *)
            err "Unknown option: $1"
            echo "Use --help for usage information"
            exit 1
            ;;
    esac
done

# Get port from env file
get_port() {
    grep "^PORT=" "${ENV_FILE}" 2>/dev/null | cut -d= -f2 || echo "8315"
}

# Get current version from binary
get_binary_version() {
    local binary_path="$1"
    if [[ -x "${binary_path}" ]]; then
        "${binary_path}" --version 2>/dev/null || echo "unknown"
    else
        echo "not found"
    fi
}

# Get version from source (VERSION file)
get_source_version() {
    if [[ -f ./VERSION ]]; then
        cat ./VERSION | tr -d '[:space:]'
    else
        echo "unknown"
    fi
}

# Health check
check_health() {
    local port="$1"
    local health
    health=$(curl -s --max-time 5 "http://localhost:${port}/health" 2>/dev/null || echo "")
    if [[ "${health}" == "OK" ]]; then
        echo "ok"
    else
        echo "fail"
    fi
}

# Service status check
check_service_status() {
    if systemctl --user is-active --quiet "${SERVICE_NAME}" 2>/dev/null; then
        echo "active"
    elif systemctl --user is-failed --quiet "${SERVICE_NAME}" 2>/dev/null; then
        echo "failed"
    else
        echo "inactive"
    fi
}

# --- Check mode: just report status and exit ---
if [[ "${CHECK_ONLY}" == true ]]; then
    echo ""
    echo "╔══════════════════════════════════════════════╗"
    echo "║ NEXUS-AI-Gateway Status Check ║"
    echo "╚══════════════════════════════════════════════╝"
    echo ""

    INSTALLED_BINARY="${HOME}/.cargo/bin/${BINARY_NAME}"
    SOURCE_BINARY="./target/release/${BINARY_NAME}"
    PORT=$(get_port)

    info "Current version (installed): $(get_binary_version "${INSTALLED_BINARY}")"
    info "Current version (built): $(get_binary_version "${SOURCE_BINARY}")"
    info "Source version (VERSION file): $(get_source_version)"

    SERVICE_STATUS=$(check_service_status)
    info "Service status: ${SERVICE_STATUS}"

    if [[ "${SERVICE_STATUS}" == "active" ]]; then
        ok "Service is running"
    elif [[ "${SERVICE_STATUS}" == "failed" ]]; then
        err "Service has failed"
    else
        warn "Service is not running"
    fi

    info "Checking health on port ${PORT}..."
    HEALTH=$(check_health "${PORT}")
    if [[ "${HEALTH}" == "ok" ]]; then
        ok "Health check: OK"
    else
        warn "Health check: FAILED (service may be starting)"
    fi

    echo ""
    exit 0
fi

# --- Start timer ---
START_TIME=$(date +%s)

# --- Pre-flight checks ---
echo ""
echo "╔══════════════════════════════════════════════╗"
echo "║ NEXUS-AI-Gateway Deploy ║"
echo "╠══════════════════════════════════════════════╣"
echo "║ Branch: ${BRANCH}$(printf '%*s' $((22 - ${#BRANCH})) " ")║"
echo "╚══════════════════════════════════════════════╝"
echo ""

info "Running pre-flight checks..."

# 1. Git repo check
if [[ ! -d .git ]]; then
    err "Not inside a git repository"
    info "Run this script from the project root directory"
    exit 1
fi
ok "Git repository detected"

# 2. Current branch check
CURRENT_BRANCH=$(git branch --show-current 2>/dev/null || echo "unknown")
if [[ "${CURRENT_BRANCH}" != "${BRANCH}" ]]; then
    err "Not on target branch: ${BRANCH} (currently on: ${CURRENT_BRANCH})"
    info "Switch branches with: git checkout ${BRANCH}"
    exit 1
fi
ok "On branch: ${BRANCH}"

# 3. Uncommitted changes check
if [[ -n "$(git status --porcelain 2>/dev/null)" ]]; then
    if [[ "${FORCE}" == true ]]; then
        warn "Uncommitted changes detected (proceeding due to --force)"
    else
        err "Uncommitted changes detected"
        info "Commit or stash changes, or use --force to skip this check"
        git status --short
        exit 1
    fi
else
    ok "Working directory is clean"
fi

# 4. Check cargo is available
if ! command -v cargo &> /dev/null; then
    err "cargo not found in PATH"
    info "Install Rust: https://rustup.rs/"
    exit 1
fi
ok "cargo is available"

# 5. Check systemctl is available
if ! command -v systemctl &> /dev/null; then
    err "systemctl not found in PATH"
    exit 1
fi
ok "systemctl is available"

# Store old version before pull
OLD_VERSION=$(get_binary_version "${HOME}/.cargo/bin/${BINARY_NAME}")
info "Current installed version: ${OLD_VERSION}"

# --- Pull latest ---
echo ""
info "Pulling latest changes from origin/${BRANCH}..."
if ! git pull origin "${BRANCH}"; then
    err "Git pull failed"
    exit 1
fi
ok "Pulled latest changes"

# Get new version from source
NEW_VERSION=$(get_source_version)
info "Source version after pull: ${NEW_VERSION}"

# --- Build release ---
echo ""
info "Building release binary (cargo build --release)..."
if ! cargo build --release; then
    err "Build failed"
    info "Service remains at old version: ${OLD_VERSION}"
    exit 1
fi
ok "Build completed"

# Verify built binary exists
BUILT_BINARY="./target/release/${BINARY_NAME}"
if [[ ! -x "${BUILT_BINARY}" ]]; then
    err "Built binary not found: ${BUILT_BINARY}"
    exit 1
fi
ok "Built binary: ${BUILT_BINARY}"

# --- Install binary ---
echo ""
info "Installing binary (cargo install --path .)..."
if ! cargo install --path .; then
    err "Installation failed"
    exit 1
fi
ok "Binary installed to ~/.cargo/bin/${BINARY_NAME}"

# --- Post-install sanity check ---
info "Running sanity checks..."
CARGO_MD5=$(md5sum "${HOME}/.cargo/bin/${BINARY_NAME}" | awk '{print $1}')
SOURCE_MD5=$(md5sum "${BUILT_BINARY}" | awk '{print $1}')

if [[ "${CARGO_MD5}" != "${SOURCE_MD5}" ]]; then
  err "CRITICAL: Binary at ~/.cargo/bin/${BINARY_NAME} does NOT match the just-built binary!"
  warn "This should never happen with cargo install. The systemd service may run an OLD version."
  warn "cargo install md5: ${CARGO_MD5}"
  warn "build output md5: ${SOURCE_MD5}"
  err "DO NOT RESTART SERVICE until this is resolved!"
  exit 1
fi
ok "Sanity check passed: ~/.cargo/bin/${BINARY_NAME} matches build output"

# Also sync to ~/.local/bin as a safeguard for users with that in PATH
mkdir -p "${HOME}/.local/bin"
LOCAL_BIN="${HOME}/.local/bin/${BINARY_NAME}"
cp "${BUILT_BINARY}" "${LOCAL_BIN}"
chmod +x "${LOCAL_BIN}"
LOCAL_MD5=$(md5sum "${LOCAL_BIN}" | awk '{print $1}')

if [[ "${LOCAL_MD5}" != "${SOURCE_MD5}" ]]; then
  warn  "~/.local/bin/${BINARY_NAME} copy has different md5sum"
else
  ok "Synced to ~/.local/bin/${BINARY_NAME} (md5 verified)"
fi

# Verify installed binary version matches source
INSTALLED_VERSION=$(get_binary_version "${HOME}/.cargo/bin/${BINARY_NAME}")
if [[ "${INSTALLED_VERSION}" != "${NEW_VERSION}" ]]; then
    warn "Version mismatch: source=${NEW_VERSION}, installed=${INSTALLED_VERSION}"
else
    ok "Version verified: ${INSTALLED_VERSION}"
fi

# Skip restart if requested
if [[ "${NO_RESTART}" == true ]]; then
    echo ""
    ok "Deployment complete (binary installed, service NOT restarted)"
    info "Version: ${OLD_VERSION} → ${INSTALLED_VERSION}"
    exit 0
fi

# --- Restart service ---
echo ""
info "Restarting service..."
systemctl --user restart "${SERVICE_NAME}"
ok "Restart command issued"

# Wait and verify
info "Waiting for service to start..."
sleep 4

SERVICE_STATUS=$(check_service_status)
if [[ "${SERVICE_STATUS}" == "active" ]]; then
    ok "Service is active"
elif [[ "${SERVICE_STATUS}" == "failed" ]]; then
    err "Service failed to start"
    echo ""
    info "Recent logs:"
    journalctl --user -u "${SERVICE_NAME}" --no-pager -n 20 || true
    exit 1
else
    warn "Service status unclear: ${SERVICE_STATUS}"
fi

# --- Health check ---
echo ""
info "Running health check..."
PORT=$(get_port)
sleep 1
HEALTH=$(check_health "${PORT}")

if [[ "${HEALTH}" == "ok" ]]; then
    ok "Health check: OK (port ${PORT})"
else
    warn "Health check: FAILED (port ${PORT})"
    info "Service may still be starting — check again in a few seconds"
fi

# --- Summary ---
ELAPSED=$(( $(date +%s) - START_TIME ))
MINUTES=$(( ELAPSED / 60 ))
SECONDS=$(( ELAPSED % 60 ))
TIME_STR=""
if [[ ${MINUTES} -gt 0 ]]; then
    TIME_STR="${MINUTES}m ${SECONDS}s"
else
    TIME_STR="${SECONDS}s"
fi

echo ""
echo "╔══════════════════════════════════════════════╗"
echo "║ Deployment Complete ║"
echo "╠══════════════════════════════════════════════╣"
printf "║ Version: %-8s → %-8s          ║\n" "${OLD_VERSION}" "${INSTALLED_VERSION}"
printf "║ Elapsed: %-36s║\n" "${TIME_STR}"
echo "╠══════════════════════════════════════════════╣"
echo "║ ║"
echo "║ Service commands: ║"
echo "║ systemctl --user status ${SERVICE_NAME} ║"
echo "║ systemctl --user restart ${SERVICE_NAME} ║"
echo "║ systemctl --user stop ${SERVICE_NAME} ║"
echo "║ journalctl --user -u ${SERVICE_NAME} -f ║"
echo "║ ║"
echo "╚══════════════════════════════════════════════╝"
echo ""
