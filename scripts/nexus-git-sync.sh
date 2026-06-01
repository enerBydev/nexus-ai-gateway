#!/bin/bash
# ═══════════════════════════════════════════════════════════════
# nexus-git-sync — Autonomous local-remote git synchronization
# ═══════════════════════════════════════════════════════════════
#
# Solves: gh pr merge / GitHub UI merge / dependabot auto-merge
# all bypass local git hooks, leaving local repo desynchronized.
#
# This daemon proactively monitors origin/main and auto-pulls
# when remote is ahead, then triggers post-merge hook for rebuild.
#
# Subcommands: watch, sync, install, uninstall, status, log
#
# Architecture: 3-layer autonomous sync
#   Layer 1: This daemon (proactive, covers ALL scenarios)
#   Layer 2: Pre-push auto-sync (reactive, faster for push scenario)
#   Layer 3: Post-merge hook (reactive, handles rebuild)
#
# Issue: https://github.com/enerBydev/nexus-ai-gateway/issues/85
# ═══════════════════════════════════════════════════════════════

set -euo pipefail

# ───────────────────────────────────────────────────────────────
# Configuration
# ───────────────────────────────────────────────────────────────

REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
LOG_FILE="/tmp/nexus-git-sync.log"
SERVICE_NAME="nexus-git-sync"
POLL_INTERVAL=60         # seconds between fetch checks
MAX_PULL_RETRIES=3       # retries if pull fails (lock contention)
RETRY_DELAY=5            # seconds between pull retries
POST_MERGE_HOOK="scripts/hooks/post-merge"
BRANCH="main"
REMOTE="origin"

# ───────────────────────────────────────────────────────────────
# Logging
# ───────────────────────────────────────────────────────────────

log() {
    local level="$1"
    shift
    local timestamp
    timestamp=$(date '+%Y-%m-%d %H:%M:%S' 2>/dev/null || echo "????-??-?? ??:??:??")
    local msg="[${timestamp}] [${level}] $*"

    # Always append to log file
    echo "$msg" >> "$LOG_FILE" 2>/dev/null || true

    # Show INFO+ on stdout when running in foreground (watch mode)
    # Suppress DEBUG unless VERBOSE is set
    case "$level" in
        DEBUG)
            if [ "${VERBOSE:-0}" = "1" ]; then
                echo "$msg"
            fi
            ;;
        INFO|WARN|ERROR)
            echo "$msg"
            ;;
    esac
}

# ───────────────────────────────────────────────────────────────
# Core sync logic: do_sync()
#
# Performs: git pull --ff-only → trigger post-merge hook
# Returns:  0 on success, 1 on failure
# ───────────────────────────────────────────────────────────────

do_sync() {
    local PULLED=false

    for attempt in $(seq 1 "$MAX_PULL_RETRIES"); do
        if git pull --ff-only "$REMOTE" "$BRANCH" 2>/dev/null; then
            local NEW_VER
            NEW_VER=$(cat VERSION 2>/dev/null | tr -d '[:space:]' || echo "unknown")
            log INFO "Synced to v${NEW_VER} (attempt ${attempt}/${MAX_PULL_RETRIES})"
            PULLED=true
            break
        else
            log WARN "Pull attempt ${attempt}/${MAX_PULL_RETRIES} failed — retrying in ${RETRY_DELAY}s"
            sleep "$RETRY_DELAY"
        fi
    done

    if [ "$PULLED" = false ]; then
        log ERROR "Pull failed after ${MAX_PULL_RETRIES} attempts — manual intervention needed"
        return 1
    fi

    # Trigger post-merge hook for rebuild + deploy
    if [ -x "$POST_MERGE_HOOK" ]; then
        log INFO "Triggering post-merge hook for rebuild..."
        # Run in background — post-merge hook does cargo build which takes minutes
        ( "$POST_MERGE_HOOK" ) &
        log INFO "Post-merge hook launched in background (PID: $!)"
    else
        log WARN "Post-merge hook not found or not executable: $POST_MERGE_HOOK"
    fi

    return 0
}

# ───────────────────────────────────────────────────────────────
# cmd_watch() — Daemon loop (Layer 1)
#
# Polls git fetch every POLL_INTERVAL seconds.
# If origin/main is ahead of local HEAD, pulls and triggers post-merge.
# ───────────────────────────────────────────────────────────────

cmd_watch() {
    cd "$REPO_DIR"

    log INFO "═══ Daemon started ═══"
    log INFO "Repository: $REPO_DIR"
    log INFO "Branch: $BRANCH | Remote: $REMOTE"
    log INFO "Poll interval: ${POLL_INTERVAL}s"
    log INFO "Post-merge hook: $POST_MERGE_HOOK"
    log INFO "════════════════════════════"

    # Flush pending sync on startup (catches changes while daemon was down)
    _flush_on_startup

    # Trap shutdown signals for clean exit
    trap 'log INFO "Shutting down gracefully..."; exit 0' SIGINT SIGTERM

    while true; do
        sleep "$POLL_INTERVAL"

        # Safety: skip if not on main branch
        local CURRENT
        CURRENT=$(git branch --show-current 2>/dev/null || echo "")
        if [ "$CURRENT" != "$BRANCH" ]; then
            log DEBUG "Not on $BRANCH (current: $CURRENT) — skipping cycle"
            continue
        fi

        # Safety: skip if git index is locked (user mid-operation)
        if [ -f ".git/index.lock" ]; then
            log DEBUG "Git index locked — skipping cycle"
            continue
        fi

        # Fetch remote refs (lightweight — only transfers ref pointers, not objects)
        if ! git fetch "$REMOTE" 2>/dev/null; then
            log WARN "git fetch failed — network issue or remote unreachable"
            continue
        fi

        # Compare local HEAD vs remote HEAD
        local LOCAL_HEAD REMOTE_HEAD
        LOCAL_HEAD=$(git rev-parse HEAD 2>/dev/null || echo "")
        REMOTE_HEAD=$(git rev-parse "remotes/${REMOTE}/${BRANCH}" 2>/dev/null || echo "")

        if [ -z "$LOCAL_HEAD" ] || [ -z "$REMOTE_HEAD" ]; then
            log WARN "Could not resolve HEAD refs — skipping cycle"
            continue
        fi

        if [ "$LOCAL_HEAD" = "$REMOTE_HEAD" ]; then
            # Already in sync — nothing to do
            continue
        fi

        # Remote is ahead — determine how many commits behind
        local BEHIND
        BEHIND=$(git rev-list --count HEAD.."${REMOTE}/${BRANCH}" 2>/dev/null || echo "?")
        log INFO "Remote is ${BEHIND} commit(s) ahead — syncing..."

        do_sync || true
    done
}

# ───────────────────────────────────────────────────────────────
# _flush_on_startup() — Check if remote is ahead and sync immediately
# ───────────────────────────────────────────────────────────────

_flush_on_startup() {
    log INFO "Checking for pending remote changes..."

    local CURRENT
    CURRENT=$(git branch --show-current 2>/dev/null || echo "")
    if [ "$CURRENT" != "$BRANCH" ]; then
        log INFO "Startup: not on $BRANCH (current: $CURRENT) — skipping flush"
        return
    fi

    if ! git fetch "$REMOTE" 2>/dev/null; then
        log WARN "Startup fetch failed — will retry on next cycle"
        return
    fi

    local LOCAL_HEAD REMOTE_HEAD
    LOCAL_HEAD=$(git rev-parse HEAD 2>/dev/null || echo "")
    REMOTE_HEAD=$(git rev-parse "remotes/${REMOTE}/${BRANCH}" 2>/dev/null || echo "")

    if [ -n "$LOCAL_HEAD" ] && [ -n "$REMOTE_HEAD" ] && [ "$LOCAL_HEAD" != "$REMOTE_HEAD" ]; then
        local BEHIND
        BEHIND=$(git rev-list --count HEAD.."${REMOTE}/${BRANCH}" 2>/dev/null || echo "?")
        log INFO "Startup: remote is ${BEHIND} commit(s) ahead — flushing sync..."
        do_sync || log WARN "Startup sync failed — will retry on next cycle"
    else
        log INFO "Startup: local and remote are in sync"
    fi
}

# ───────────────────────────────────────────────────────────────
# cmd_sync() — One-shot immediate sync (no daemon loop)
# ───────────────────────────────────────────────────────────────

cmd_sync() {
    cd "$REPO_DIR"

    local CURRENT
    CURRENT=$(git branch --show-current 2>/dev/null || echo "")
    if [ "$CURRENT" != "$BRANCH" ]; then
        echo "Not on $BRANCH branch (current: $CURRENT) — skipping"
        exit 0
    fi

    echo "Performing one-shot sync..."
    git fetch "$REMOTE" 2>/dev/null || {
        echo "ERROR: git fetch failed"
        exit 1
    }

    local LOCAL_HEAD REMOTE_HEAD
    LOCAL_HEAD=$(git rev-parse --short HEAD 2>/dev/null || echo "unknown")
    REMOTE_HEAD=$(git rev-parse --short "remotes/${REMOTE}/${BRANCH}" 2>/dev/null || echo "unknown")

    echo "Local:  $LOCAL_HEAD"
    echo "Remote: $REMOTE_HEAD"

    if [ "$LOCAL_HEAD" = "$REMOTE_HEAD" ]; then
        echo "Already in sync — nothing to do"
        exit 0
    fi

    local BEHIND
    BEHIND=$(git rev-list --count HEAD.."${REMOTE}/${BRANCH}" 2>/dev/null || echo "?")
    echo "Remote is ${BEHIND} commit(s) ahead — pulling..."

    do_sync
    local result=$?
    if [ $result -eq 0 ]; then
        local NEW_VER
        NEW_VER=$(cat VERSION 2>/dev/null | tr -d '[:space:]' || echo "unknown")
        echo "Sync complete: v${NEW_VER}"
    else
        echo "ERROR: Sync failed"
        exit 1
    fi
}

# ───────────────────────────────────────────────────────────────
# cmd_install() — Install systemd user service
# ───────────────────────────────────────────────────────────────

cmd_install() {
    local SYSTEMD_DIR="$HOME/.config/systemd/user"
    mkdir -p "$SYSTEMD_DIR"

    # Generate service unit file
    cat > "${SYSTEMD_DIR}/${SERVICE_NAME}.service" << EOF
[Unit]
Description=NEXUS Git Sync — Autonomous local-remote synchronization
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart=/bin/bash ${REPO_DIR}/scripts/nexus-git-sync.sh watch
Restart=on-failure
RestartSec=30
StandardOutput=append:${LOG_FILE}
StandardError=append:${LOG_FILE}

# Security: run as the current user (systemd user service already does this)
# ProtectSystem=strict is not available for user services

[Install]
WantedBy=default.target
EOF

    if ! systemctl --user daemon-reload 2>/dev/null; then
        echo "ERROR: systemd user instance not available (running in CI/container?)"
        echo "  Install skipped — daemon will work in foreground mode: $0 watch"
        return 1
    fi
    systemctl --user enable "${SERVICE_NAME}.service"
    systemctl --user start "${SERVICE_NAME}.service"

    echo ""
    echo "══════════════════════════════════════════════════════"
    echo "  NEXUS Git Sync daemon installed and started"
    echo "══════════════════════════════════════════════════════"
    echo ""
    echo "  Service:  ${SERVICE_NAME}.service"
    echo "  Log:      ${LOG_FILE}"
    echo "  Interval: ${POLL_INTERVAL}s"
    echo ""
    systemctl --user status "${SERVICE_NAME}.service" --no-pager 2>/dev/null || true
}

# ───────────────────────────────────────────────────────────────
# cmd_uninstall() — Remove systemd user service
# ───────────────────────────────────────────────────────────────

cmd_uninstall() {
    systemctl --user stop "${SERVICE_NAME}.service" 2>/dev/null || true
    systemctl --user disable "${SERVICE_NAME}.service" 2>/dev/null || true
    rm -f "$HOME/.config/systemd/user/${SERVICE_NAME}.service"
    systemctl --user daemon-reload

    echo ""
    echo "══════════════════════════════════════════════════════"
    echo "  NEXUS Git Sync daemon uninstalled"
    echo "══════════════════════════════════════════════════════"
    echo ""
    echo "  Log file preserved at: ${LOG_FILE}"
    echo "  Remove with: rm ${LOG_FILE}"
}

# ───────────────────────────────────────────────────────────────
# cmd_status() — Show daemon status and sync state
# ───────────────────────────────────────────────────────────────

cmd_status() {
    echo "══════════════════════════════════════════════════════"
    echo "  NEXUS Git Sync — Status"
    echo "══════════════════════════════════════════════════════"
    echo ""

    # Service status
    echo "── Service ──"
    if systemctl --user is-active --quiet "${SERVICE_NAME}.service" 2>/dev/null; then
        echo "  Status:   ACTIVE (running)"
        systemctl --user status "${SERVICE_NAME}.service" --no-pager 2>/dev/null | grep -E '(Active|Main PID|Tasks)' || true
    else
        echo "  Status:   INACTIVE (not running)"
        echo "  Install:  task git-sync-install"
    fi
    echo ""

    # Sync state
    echo "── Sync State ──"
    cd "$REPO_DIR" 2>/dev/null || {
        echo "  Cannot access repo: $REPO_DIR"
        return
    }

    local LOCAL_HEAD REMOTE_HEAD BEHIND AHEAD
    LOCAL_HEAD=$(git rev-parse --short HEAD 2>/dev/null || echo "unknown")
    REMOTE_HEAD=$(git rev-parse --short "remotes/${REMOTE}/${BRANCH}" 2>/dev/null || echo "unknown (fetch first)")
    BEHIND=$(git rev-list --count HEAD.."${REMOTE}/${BRANCH}" 2>/dev/null || echo "?")
    AHEAD=$(git rev-list --count "${REMOTE}/${BRANCH}"..HEAD 2>/dev/null || echo "?")

    echo "  Local:    $LOCAL_HEAD"
    echo "  Remote:   $REMOTE_HEAD"
    echo "  Behind:   ${BEHIND} commit(s)"
    echo "  Ahead:    ${AHEAD} commit(s)"

    local CURRENT
    CURRENT=$(git branch --show-current 2>/dev/null || echo "unknown")
    echo "  Branch:   $CURRENT"
    echo ""

    # Version
    local VER
    VER=$(cat VERSION 2>/dev/null | tr -d '[:space:]' || echo "unknown")
    echo "  Version:  v${VER}"
    echo ""

    # Recent log
    echo "── Recent Log (last 10 lines) ──"
    if [ -f "$LOG_FILE" ]; then
        tail -10 "$LOG_FILE" 2>/dev/null
    else
        echo "  No log file at $LOG_FILE"
    fi
}

# ───────────────────────────────────────────────────────────────
# cmd_log() — Follow the sync log in real-time
# ───────────────────────────────────────────────────────────────

cmd_log() {
    if [ -f "$LOG_FILE" ]; then
        echo "Following ${LOG_FILE} (Ctrl+C to stop)..."
        tail -f "$LOG_FILE"
    else
        echo "No log file at $LOG_FILE"
        echo "The daemon will create it on first run."
    fi
}

# ───────────────────────────────────────────────────────────────
# cmd_help() — Show usage information
# ───────────────────────────────────────────────────────────────

cmd_help() {
    cat << EOF
nexus-git-sync — Autonomous local-remote git synchronization

Solves the desync problem where gh pr merge, GitHub UI merges,
and dependabot auto-merges bypass local git hooks, leaving the
local repository behind remote with no automatic correction.

This is Layer 1 of the 3-layer autonomous sync system:
  Layer 1: This daemon (proactive, polls every ${POLL_INTERVAL}s)
  Layer 2: Pre-push auto-sync (reactive, 6x60s retries)
  Layer 3: Post-merge hook (reactive, rebuild + deploy)

Usage:
  $0 <command>

Commands:
  watch      Run daemon loop (polls origin/main every ${POLL_INTERVAL}s)
  sync       One-shot immediate sync (no daemon)
  install    Install + enable systemd user service
  uninstall  Remove systemd user service
  status     Show daemon status and sync state
  log        Follow sync log in real-time
  help       Show this help message

Environment:
  VERBOSE=1  Enable DEBUG-level log output

Examples:
  $0 watch                       # Run daemon in foreground
  $0 sync                        # Pull if remote is ahead
  $0 install                     # Install as systemd service
  $0 status                      # Check sync state
  $0 log                         # Follow log output

Issue: https://github.com/enerBydev/nexus-ai-gateway/issues/85
EOF
}

# ───────────────────────────────────────────────────────────────
# Dispatch
# ───────────────────────────────────────────────────────────────

case "${1:-help}" in
    watch)      cmd_watch ;;
    sync)       cmd_sync ;;
    install)    cmd_install ;;
    uninstall)  cmd_uninstall ;;
    status)     cmd_status ;;
    log)        cmd_log ;;
    help|--help|-h)
        cmd_help
        ;;
    *)
        echo "ERROR: Unknown command '$1'"
        echo ""
        cmd_help
        exit 1
        ;;
esac
