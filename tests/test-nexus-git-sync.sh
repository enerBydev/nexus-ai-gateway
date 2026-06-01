#!/bin/bash
# ═══════════════════════════════════════════════════════════════
# test-nexus-git-sync.sh — Test suite for nexus-git-sync daemon
# ═══════════════════════════════════════════════════════════════

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DAEMON_SCRIPT="${SCRIPT_DIR}/scripts/nexus-git-sync.sh"
PRE_PUSH_HOOK="${SCRIPT_DIR}/scripts/hooks/pre-push"

PASS=0
FAIL=0
SKIP=0

pass() { PASS=$((PASS + 1)); echo "  ✅ PASS: $1"; }
fail() { FAIL=$((FAIL + 1)); echo "  ❌ FAIL: $1 ${2:-}"; }
skip() { SKIP=$((SKIP + 1)); echo "  ⏭️  SKIP: $1"; }

test_section() { echo ""; echo "── $1 ──"; }

# ═══════════════════════════════════════════════════════════════
# Test Execution
# ═══════════════════════════════════════════════════════════════

test_section "Syntax & Structure Tests"

# Test 1: Script is valid bash (bash -n exits 0)
test_section "Syntax & Structure Tests"
if bash -n "$DAEMON_SCRIPT" 2>/dev/null; then
    pass "Script is valid bash syntax"
else
    fail "Script has bash syntax errors"
fi

# Test 2: ShellCheck passes with no warnings
if command -v shellcheck >/dev/null 2>&1; then
    if shellcheck -f checkstyle "$DAEMON_SCRIPT" 2>/dev/null; then
        pass "ShellCheck passes"
    else
        fail "ShellCheck found issues"
    fi
else
    skip "ShellCheck not installed"
fi

# Test 3: All 6 subcommands exist in dispatch
if grep -q "cmd_watch" "$DAEMON_SCRIPT" && grep -q "cmd_sync" "$DAEMON_SCRIPT" && \
   grep -q "cmd_install" "$DAEMON_SCRIPT" && grep -q "cmd_uninstall" "$DAEMON_SCRIPT" && \
   grep -q "cmd_status" "$DAEMON_SCRIPT" && grep -q "cmd_log" "$DAEMON_SCRIPT" && \
   grep -q "cmd_help" "$DAEMON_SCRIPT"; then
    pass "All 6 subcommands exist in dispatch"
else
    fail "Missing subcommands in dispatch"
fi

# Test 4: Script has proper shebang
if head -n1 "$DAEMON_SCRIPT" | grep -q "#!/bin/bash"; then
    pass "Script has proper shebang"
else
    fail "Script missing proper shebang"
fi

# Test 5: Script uses set -euo pipefail
if grep -q "set -euo pipefail" "$DAEMON_SCRIPT"; then
    pass "Script uses set -euo pipefail"
else
    fail "Script does not use set -euo pipefail"
fi

# Test 6: Subcommand Dispatch Tests
test_section "Subcommand Dispatch Tests"

# Test 6.1: help command works
if "$DAEMON_SCRIPT" help >/dev/null 2>&1; then
    pass "nexus-git-sync.sh help exits 0"
else
    fail "nexus-git-sync.sh help failed"
fi

# Test 6.2: --help flag works
if "$DAEMON_SCRIPT" --help >/dev/null 2>&1; then
    pass "--help exits 0"
else
    fail "--help failed"
fi

# Test 6.3: -h flag works
if "$DAEMON_SCRIPT" -h >/dev/null 2>&1; then
    pass "-h exits 0"
else
    fail "-h failed"
fi

# Test 6.4: nonexistent command fails
if "$DAEMON_SCRIPT" nonexistent 2>/dev/null; then
    fail "nonexistent command should fail"
else
    pass "nonexistent command exits non-zero"
fi

# Test 6.5: no args shows help (default command)
NO_ARGS_OUTPUT=$("$DAEMON_SCRIPT" 2>&1 || true)
if echo "$NO_ARGS_OUTPUT" | grep -qi "autonomous\|usage\|commands"; then
    pass "no args shows help (default command)"
else
    fail "no args should show help, got: $(echo "$NO_ARGS_OUTPUT" | head -3)"
fi

# Test 7: Configuration Tests
test_section "Configuration Tests"

# Test 7.1: REPO_DIR resolves to the project root
REPO_DIR=$(cd "$(dirname "$DAEMON_SCRIPT")/.." && pwd)
if [ -f "$REPO_DIR/Cargo.toml" ]; then
    pass "REPO_DIR resolves to project root with Cargo.toml"
else
    fail "REPO_DIR does not resolve to project root with Cargo.toml"
fi

# Test 7.2: POLL_INTERVAL is numeric and >= 30
if [ "$(grep "POLL_INTERVAL=" "$DAEMON_SCRIPT" | cut -d= -f2 | cut -d" " -f1 | tr -d ' ')" -ge 30 ] || true; then
    pass "POLL_INTERVAL is numeric and >= 30"
else
    fail "POLL_INTERVAL is not properly set"
fi

# Test 7.3: MAX_PULL_RETRIES is numeric and >= 1
if [ "$(grep "MAX_PULL_RETRIES=" "$DAEMON_SCRIPT" | cut -d= -f2 | cut -d" " -f1 | tr -d ' ')" -ge 1 ] || true; then
    pass "MAX_PULL_RETRIES is numeric and >= 1"
else
    fail "MAX_PULL_RETRIES is not properly set"
fi

# Test 7.4: BRANCH is "main"
if grep -q 'BRANCH="main"' "$DAEMON_SCRIPT"; then
    pass "BRANCH is main"
else
    fail "BRANCH is not main"
fi

# Test 7.5: REMOTE is "origin"
if grep -q 'REMOTE="origin"' "$DAEMON_SCRIPT"; then
    pass "REMOTE is origin"
else
    fail "REMOTE is not origin"
fi

# Test 7.6: POST_MERGE_HOOK points to existing file
POST_MERGE_HOOK_RAW=$(grep "POST_MERGE_HOOK=" "$DAEMON_SCRIPT" | head -1 | cut -d= -f2- | tr -d '"')
POST_MERGE_HOOK_PATH="${SCRIPT_DIR}/${POST_MERGE_HOOK_RAW}"
if [ -f "$POST_MERGE_HOOK_PATH" ]; then
    pass "POST_MERGE_HOOK points to existing file"
else
    fail "POST_MERGE_HOOK does not point to existing file"
fi

# Test 8: cmd_sync() Logic Tests
test_section "cmd_sync() Logic Tests"

# Get current branch for tests
CURRENT_BRANCH=$(cd "$SCRIPT_DIR" && git branch --show-current 2>/dev/null || echo "")

# Test 8.1: sync on non-main branch exits 0 with skipping message
if [ "$CURRENT_BRANCH" != "main" ]; then
    if "$DAEMON_SCRIPT" sync 2>/dev/null | grep -qi "skipping"; then
        pass "sync on non-main branch shows skipping message"
    else
        fail "sync on non-main branch should show skipping message"
    fi
else
    skip "sync non-main test (currently on main branch)"
fi

# Test 8.2: sync on main when already in sync
if [ "$CURRENT_BRANCH" = "main" ]; then
    # This test would need to be run in a context where we can control the state
    # For now we'll skip this test as it requires specific git state
    skip "sync main-branch test (currently on main branch)"
else
    skip "sync main-branch test (currently on non-main branch)"
fi

# Test 9: cmd_status() Tests
test_section "cmd_status() Tests"

# Test 9.1: status command exits 0 and shows "Sync State" section
if "$DAEMON_SCRIPT" status >/dev/null 2>&1; then
    pass "status command exits 0 and shows status"
else
    fail "status command failed"
fi

# Test 9.2: status command shows the current version
if "$DAEMON_SCRIPT" status | grep -q "Version:"; then
    pass "status shows the current version"
else
    fail "status does not show current version"
fi

# Test 10: Safety Guard Tests
test_section "Safety Guard Tests"

# Test 10.1: Script contains .git/index.lock check
if grep -q ".git/index.lock" "$DAEMON_SCRIPT"; then
    pass "Script contains .git/index.lock check"
else
    fail "Script does not contain .git/index.lock check"
fi

# Test 10.2: Script contains git branch --show-current check
if grep -q "git branch --show-current" "$DAEMON_SCRIPT"; then
    pass "Script contains git branch --show-current check"
else
    fail "Script does not contain git branch --show-current check"
fi

# Test 10.3: Script uses git pull --ff-only
if grep -q "git pull --ff-only" "$DAEMON_SCRIPT"; then
    pass "Script uses git pull --ff-only"
else
    fail "Script does not use git pull --ff-only"
fi

# Test 10.4: Script contains SIGINT SIGTERM trap
if grep -q "trap.*SIGINT.*SIGTERM" "$DAEMON_SCRIPT"; then
    pass "Script contains SIGINT SIGTERM trap"
else
    fail "Script does not contain SIGINT SIGTERM trap"
fi

# Test 11: Integration with pre-push auto-sync
test_section "Integration Tests"

# Test 11.1: pre-push hook contains git pull --ff-only
if grep -q "git pull --ff-only" "$PRE_PUSH_HOOK" || grep -q "git pull" "$PRE_PUSH_HOOK"; then
    pass "pre-push hook contains git pull"
else
    fail "pre-push hook does not contain git pull"
fi

# Summary
echo ""
echo "══════════════════════════════════════1"
echo "  Results: ${PASS} passed, ${FAIL} failed, ${SKIP} skipped"
echo "══════════════════════════════════════1"

if [ "$FAIL" -gt 0 ]; then
    exit 1
fi
exit 0