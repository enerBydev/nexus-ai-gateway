#!/usr/bin/env bash
# smoke-test-graceful-shutdown.sh — Verify NEXUS-AI-Gateway graceful shutdown lifecycle
#
# Usage:
#   ./scripts/smoke-test-graceful-shutdown.sh              # Default: cargo run on port 8315
#   PORT=9999 ./scripts/smoke-test-graceful-shutdown.sh    # Custom port
#   BINARY=~/.cargo/bin/nexus-ai-gateway ./scripts/...    # Use installed binary
#   DRAIN_TIMEOUT_SECS=60 ./scripts/...                   # Custom drain timeout
#
# Prerequisites:
#   - UPSTREAM_BASE_URL and UPSTREAM_API_KEY set in ~/.nexus-ai-gateway.env or environment
#   - curl installed
#   - cargo (if using default BINARY=cargo-run)
#
# What it tests:
#   1. Server starts and /health returns 200
#   2. SIGTERM triggers drain mode
#   3. /health returns 503 during drain
#   4. Process exits within DRAIN_TIMEOUT_SECS + margin
#
set -euo pipefail

# ─── Configuration ───────────────────────────────────────────────────────────
PORT="${PORT:-8315}"
DRAIN_TIMEOUT_SECS="${DRAIN_TIMEOUT_SECS:-30}"
BINARY="${BINARY:-cargo-run}"
HEALTH_URL="http://127.0.0.1:${PORT}/health"
MARGIN_SECS=15
MAX_WAIT_SECS=$((DRAIN_TIMEOUT_SECS + MARGIN_SECS))
STARTUP_TIMEOUT=10

# ─── Colors ──────────────────────────────────────────────────────────────────
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
CYAN='\033[0;36m'
NC='\033[0m' # No Color

# ─── State ───────────────────────────────────────────────────────────────────
PID=""
PASSED=0
FAILED=0

# ─── Cleanup ─────────────────────────────────────────────────────────────────
cleanup() {
    if [[ -n "${PID}" ]] && kill -0 "${PID}" 2>/dev/null; then
        echo -e "${YELLOW}[CLEANUP]${NC} Killing leftover process ${PID}"
        kill -9 "${PID}" 2>/dev/null || true
        wait "${PID}" 2>/dev/null || true
    fi
}
trap cleanup EXIT

# ─── Helpers ─────────────────────────────────────────────────────────────────
pass() { PASSED=$((PASSED + 1)); echo -e "  ${GREEN}✅ PASS${NC}: $1"; }
fail() { FAILED=$((FAILED + 1)); echo -e "  ${RED}❌ FAIL${NC}: $1"; }
info() { echo -e "  ${CYAN}ℹ️${NC} $1"; }
warn() { echo -e "  ${YELLOW}⚠️${NC} $1"; }

http_status() {
    curl -s -o /dev/null -w '%{http_code}' --max-time 5 "$1" 2>/dev/null || echo "000"
}

# ─── Load environment ────────────────────────────────────────────────────────
ENV_FILE="${HOME}/.nexus-ai-gateway.env"
if [[ -f "${ENV_FILE}" ]]; then
    # shellcheck disable=SC1090
    set -a; source "${ENV_FILE}"; set +a
    info "Loaded environment from ${ENV_FILE}"
else
    warn "No .env file at ${ENV_FILE} — ensure UPSTREAM_BASE_URL and UPSTREAM_API_KEY are set"
fi

# ─── Verify prerequisites ───────────────────────────────────────────────────
if ! command -v curl &>/dev/null; then
    echo -e "${RED}ERROR${NC}: curl is required but not installed"
    exit 1
fi

echo ""
echo -e "${CYAN}═══════════════════════════════════════════════════════════════${NC}"
echo -e "${CYAN}  NEXUS-AI-Gateway — Graceful Shutdown Smoke Test${NC}"
echo -e "${CYAN}═══════════════════════════════════════════════════════════════${NC}"
echo -e "  Port:              ${PORT}"
echo -e "  Drain timeout:     ${DRAIN_TIMEOUT_SECS}s"
echo -e "  Max wait:          ${MAX_WAIT_SECS}s"
echo -e "  Binary:            ${BINARY}"
echo -e "  Health URL:        ${HEALTH_URL}"
echo ""

# ─── Test 1: Start server ────────────────────────────────────────────────────
echo -e "${YELLOW}[TEST 1]${NC} Starting NEXUS-AI-Gateway..."

if [[ "${BINARY}" == "cargo-run" ]]; then
    PORT="${PORT}" cargo run 2>/dev/null &
else
    PORT="${PORT}" "${BINARY}" 2>/dev/null &
fi
PID=$!
info "Started with PID ${PID}"

# Wait for server to become healthy
elapsed=0
while [[ ${elapsed} -lt ${STARTUP_TIMEOUT} ]]; do
    status=$(http_status "${HEALTH_URL}")
    if [[ "${status}" == "200" ]]; then
        break
    fi
    sleep 1
    elapsed=$((elapsed + 1))
done

if [[ "${status}" == "200" ]]; then
    pass "Server started and /health returned 200 (${elapsed}s)"
else
    fail "Server did not become healthy within ${STARTUP_TIMEOUT}s (last status: ${status})"
    exit 1
fi

# ─── Test 2: Verify /health returns 200 during normal operation ─────────────
echo -e "${YELLOW}[TEST 2]${NC} Verifying /health returns 200 during normal operation..."
status=$(http_status "${HEALTH_URL}")
if [[ "${status}" == "200" ]]; then
    pass "/health returned 200 during normal operation"
else
    fail "/health returned ${status} (expected 200)"
fi

# ─── Test 3: Send SIGTERM and verify drain mode ─────────────────────────────
echo -e "${YELLOW}[TEST 3]${NC} Sending SIGTERM to PID ${PID}..."
kill -TERM "${PID}" 2>/dev/null

# Give the process a moment to set IS_DRAINING=true
sleep 2

status=$(http_status "${HEALTH_URL}")
if [[ "${status}" == "503" ]]; then
    pass "/health returned 503 during drain (IS_DRAINING working)"
else
    # Process may have already exited (fast drain) — that's also acceptable
    if ! kill -0 "${PID}" 2>/dev/null; then
        pass "Process exited quickly (no active connections to drain)"
    else
        fail "/health returned ${status} during drain (expected 503)"
    fi
fi

# ─── Test 4: Verify process exits within timeout ────────────────────────────
echo -e "${YELLOW}[TEST 4]${NC} Waiting for process to exit (max ${MAX_WAIT_SECS}s)..."

elapsed=0
while [[ ${elapsed} -lt ${MAX_WAIT_SECS} ]]; do
    if ! kill -0 "${PID}" 2>/dev/null; then
        break
    fi
    sleep 1
    elapsed=$((elapsed + 1))
done

if ! kill -0 "${PID}" 2>/dev/null; then
    if [[ ${elapsed} -le ${DRAIN_TIMEOUT_SECS} ]]; then
        pass "Process exited within drain timeout (${elapsed}s <= ${DRAIN_TIMEOUT_SECS}s)"
    else
        pass "Process exited within max wait (${elapsed}s <= ${MAX_WAIT_SECS}s, exceeded drain timeout by $((elapsed - DRAIN_TIMEOUT_SECS))s)"
    fi
    wait "${PID}" 2>/dev/null || true
    PID=""
else
    fail "Process did not exit within ${MAX_WAIT_SECS}s — killing"
    kill -9 "${PID}" 2>/dev/null || true
    wait "${PID}" 2>/dev/null || true
    PID=""
fi

# ─── Summary ─────────────────────────────────────────────────────────────────
echo ""
echo -e "${CYAN}═══════════════════════════════════════════════════════════════${NC}"
echo -e "${CYAN}  Results: ${GREEN}${PASSED} passed${NC}, ${RED}${FAILED} failed${NC}"
echo -e "${CYAN}═══════════════════════════════════════════════════════════════${NC}"
echo ""

if [[ ${FAILED} -eq 0 ]]; then
    echo -e "${GREEN}🎉 All smoke tests passed! Graceful shutdown is working correctly.${NC}"
    exit 0
else
    echo -e "${RED}💥 Some tests failed. Review the output above.${NC}"
    exit 1
fi
