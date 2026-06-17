#!/usr/bin/env bash
# sync-from-release.sh — Reconcile the installed binary with the latest GitHub Release.
#
# Why this exists
# ---------------
# The post-merge git hook only rebuilds+installs when (a) you are ON the main branch
# AND (b) a `git pull` brings a version change. The CI auto-version job bumps VERSION
# in a separate "[skip ci]" commit AFTER the feature merge, so that bump is frequently
# fetched while you are on a feature branch (or via a plain `git fetch`) — the hook
# never fires and the installed binary lags the published Release (e.g. prod 0.21.0 vs
# Release 0.21.1).
#
# This script reconciles the installed binary to the latest Release, independent of the
# current branch, idempotently. It PREFERS the CI-built Release asset, but that asset is
# only usable if it runs on this host's glibc — a CI runner newer than prod can emit a
# binary that needs a newer glibc than prod has (the GLIBC_2.39 problem). So the script
# verifies the downloaded binary actually executes here; if it does not (or the download
# fails), it falls back to a local `cargo build --release`, which always links this
# host's glibc.
#
# Default mode is DRY-RUN: it reports state and exits WITHOUT changing anything. Pass
# --apply to act. It never touches prod unless every verification passes.
set -uo pipefail

REPO="enerBydev/nexus-ai-gateway"
BINARY_NAME="nexus-ai-gateway"
CARGO_BIN="$HOME/.cargo/bin/$BINARY_NAME"
LOCAL_BIN="$HOME/.local/bin/$BINARY_NAME"
ENV_FILE="$HOME/.nexus-ai-gateway.env"
SERVICE="nexus-ai-gateway"

APPLY=false
RESTART=true
FORCE=false

usage() {
  cat <<EOF
sync-from-release.sh — install the latest GitHub Release binary (glibc-safe, branch-independent)

Usage: $0 [--apply] [--no-restart] [--force] [-h]

  (no flags)    DRY-RUN: report installed vs latest, make NO changes (default)
  --apply       Download/build + install the latest release version
  --no-restart  With --apply: install but do NOT restart the systemd service
  --force       Reinstall even if the installed version already matches
  -h, --help    Show this help
EOF
}

for arg in "$@"; do
  case "$arg" in
    --apply) APPLY=true ;;
    --no-restart) RESTART=false ;;
    --force) FORCE=true ;;
    -h | --help)
      usage
      exit 0
      ;;
    *)
      echo "Unknown arg: $arg" >&2
      usage
      exit 2
      ;;
  esac
done

installed_version() {
  if [ -x "$CARGO_BIN" ]; then
    "$CARGO_BIN" --version 2>/dev/null | awk '{print $2}'
  fi
}

INSTALLED="$(installed_version)"
[ -z "$INSTALLED" ] && INSTALLED="none"

if ! command -v gh >/dev/null 2>&1; then
  echo "❌ gh CLI not found — cannot query releases" >&2
  exit 1
fi

LATEST_TAG="$(gh release view --repo "$REPO" --json tagName -q .tagName 2>/dev/null || true)"
if [ -z "$LATEST_TAG" ]; then
  echo "❌ Could not determine latest release tag for $REPO" >&2
  exit 1
fi
LATEST_VER="${LATEST_TAG#v}"

echo "Repo:            $REPO"
echo "Installed:       $INSTALLED  ($CARGO_BIN)"
echo "Latest release:  $LATEST_VER ($LATEST_TAG)"

if [ "$INSTALLED" = "$LATEST_VER" ] && [ "$FORCE" = false ]; then
  echo "✅ Up to date — nothing to do."
  exit 0
fi

if [ "$APPLY" = false ]; then
  echo ""
  echo "→ Update available: $INSTALLED → $LATEST_VER"
  echo "  Re-run with --apply to install (dry-run made no changes)."
  exit 0
fi

# ---------------------------------------------------------------------------
# APPLY path
# ---------------------------------------------------------------------------
TMP="$(mktemp -d)"
cleanup() { rm -rf "$TMP"; }
trap cleanup EXIT

STAGED="$TMP/$BINARY_NAME"
SOURCE_DESC=""

echo ""
echo "⬇  Downloading Release asset $LATEST_TAG…"
if gh release download "$LATEST_TAG" --repo "$REPO" -p "$BINARY_NAME" -D "$TMP" --clobber 2>/dev/null; then
  chmod +x "$STAGED" 2>/dev/null || true
  DL_VER="$("$STAGED" --version 2>/dev/null | awk '{print $2}')"
  if [ "$DL_VER" = "$LATEST_VER" ]; then
    SOURCE_DESC="CI Release asset ($LATEST_TAG)"
    echo "✅ Release asset runs on this host and reports v$DL_VER"
  else
    echo "⚠  Release asset is not usable here (glibc mismatch or wrong version: '${DL_VER:-<none>}')."
    STAGED="" # force local-build fallback
  fi
else
  echo "⚠  Release asset download failed."
  STAGED=""
fi

# Fallback: build locally against this host's glibc.
if [ -z "$STAGED" ]; then
  echo ""
  echo "🔨 Falling back to local build (cargo build --release)…"
  REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
  if ! (cd "$REPO_DIR" && cargo build --release); then
    echo "❌ Local build failed — prod NOT changed." >&2
    exit 1
  fi
  BUILT="$REPO_DIR/target/release/$BINARY_NAME"
  BUILT_VER="$("$BUILT" --version 2>/dev/null | awk '{print $2}')"
  if [ "$BUILT_VER" != "$LATEST_VER" ]; then
    echo "❌ Built v${BUILT_VER:-<none>} != latest v$LATEST_VER — your checkout is behind." >&2
    echo "   Run 'git checkout main && git pull' first. prod NOT changed." >&2
    exit 1
  fi
  STAGED="$BUILT"
  SOURCE_DESC="local build (host glibc $(ldd --version 2>/dev/null | awk 'NR==1{print $NF}'))"
fi

# Install to both canonical paths + md5 cross-verify (mirrors 'task sync-binary').
mkdir -p "$(dirname "$CARGO_BIN")" "$(dirname "$LOCAL_BIN")"
install -m 0755 "$STAGED" "$CARGO_BIN"
install -m 0755 "$STAGED" "$LOCAL_BIN"
SRC_MD5="$(md5sum "$STAGED" | awk '{print $1}')"
CARGO_MD5="$(md5sum "$CARGO_BIN" | awk '{print $1}')"
LOCAL_MD5="$(md5sum "$LOCAL_BIN" | awk '{print $1}')"
if [ "$SRC_MD5" != "$CARGO_MD5" ] || [ "$SRC_MD5" != "$LOCAL_MD5" ]; then
  echo "❌ md5 mismatch after install — prod binary may be corrupt. NOT restarting." >&2
  exit 1
fi
echo "✅ Installed v$LATEST_VER from $SOURCE_DESC (md5 ${SRC_MD5:0:8})"

if [ "$RESTART" = false ]; then
  echo "↩  --no-restart: run 'systemctl --user restart $SERVICE' when ready."
  exit 0
fi

echo ""
echo "🔄 Restarting $SERVICE…"
if ! systemctl --user restart "$SERVICE"; then
  echo "❌ Service restart failed." >&2
  exit 1
fi
sleep 3
PORT="$(grep -E '^PORT=' "$ENV_FILE" 2>/dev/null | cut -d= -f2 | tr -d '[:space:]')"
PORT="${PORT:-8315}"
if [ "$(curl -s --max-time 5 "http://localhost:${PORT}/health" 2>/dev/null)" = "OK" ]; then
  echo "✅ Deploy complete — v$LATEST_VER healthy on :$PORT"
else
  echo "⚠  Service restarted but /health did not return OK on :$PORT — check 'task service-logs'." >&2
  exit 1
fi
