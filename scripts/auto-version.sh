#!/bin/bash
# auto-version.sh — Automatic version management based on conventional commits
#
# This script analyzes commits since the last version tag and determines
# the appropriate version bump (major/minor/patch) based on commit types.
#
# Called from:
#   1. post-commit hook (analyzes pending commits, shows recommended bump)
#   2. GitHub Actions CI (auto-bumps before release)
#   3. Manually: ./scripts/auto-version.sh [--dry-run|--apply]
#
# Logic (from ESTRATEGIA_VERSIONADO_v0xx.md section 4):
#   feat:      → MINOR bump (nueva feature)
#   fix:       → PATCH bump (bug fix)
#   refactor:  → PATCH bump (mejora interna)
#   perf:      → PATCH bump (optimización)
#   docs:      → SIN BUMP
#   chore:     → SIN BUMP
#   test:      → SIN BUMP
#   ci:        → SIN BUMP
#   build:     → SIN BUMP
#   style:     → SIN BUMP
#   BREAKING:  → MINOR bump (while v0.x.x)

set -e

PROJECT_ROOT="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"
cd "$PROJECT_ROOT"

MODE="${1:---dry-run}"  # default: dry-run

# Get current version
CURRENT_VERSION=$(cat VERSION 2>/dev/null || grep '^version' Cargo.toml | sed 's/version = "\(.*\)"/\1/')
CURRENT_TAG="v${CURRENT_VERSION}"

# Check if tag exists
if ! git rev-parse "$CURRENT_TAG" >/dev/null 2>&1; then
    echo "⚠️  Tag $CURRENT_TAG not found. Using all commits."
    RANGE="HEAD"
else
    RANGE="${CURRENT_TAG}..HEAD"
fi

# Count commit types since last tag
COMMITS=$(git log "$RANGE" --oneline 2>/dev/null)
TOTAL=$(echo "$COMMITS" | grep -c '.' 2>/dev/null || echo 0)

if [ "$TOTAL" -eq 0 ]; then
    echo "✅ No new commits since $CURRENT_TAG — no bump needed"
    exit 0
fi

# Analyze commit types
FEAT_COUNT=$(echo "$COMMITS" | grep -cE '^[a-f0-9]+ feat' || true)
FIX_COUNT=$(echo "$COMMITS" | grep -cE '^[a-f0-9]+ fix' || true)
REFACTOR_COUNT=$(echo "$COMMITS" | grep -cE '^[a-f0-9]+ refactor' || true)
PERF_COUNT=$(echo "$COMMITS" | grep -cE '^[a-f0-9]+ perf' || true)
BREAKING_COUNT=$(echo "$COMMITS" | grep -cE '!:' || true)

# Non-bumping commits
CHORE_COUNT=$(echo "$COMMITS" | grep -cE '^[a-f0-9]+ chore' || true)
DOCS_COUNT=$(echo "$COMMITS" | grep -cE '^[a-f0-9]+ docs' || true)
CI_COUNT=$(echo "$COMMITS" | grep -cE '^[a-f0-9]+ ci' || true)
TEST_COUNT=$(echo "$COMMITS" | grep -cE '^[a-f0-9]+ test' || true)

BUMP_COMMITS=$((FEAT_COUNT + FIX_COUNT + REFACTOR_COUNT + PERF_COUNT))
NO_BUMP_COMMITS=$((CHORE_COUNT + DOCS_COUNT + CI_COUNT + TEST_COUNT))

# Determine bump level
BUMP="none"
if [ "$BREAKING_COUNT" -gt 0 ] || [ "$FEAT_COUNT" -gt 0 ]; then
    BUMP="minor"
elif [ "$FIX_COUNT" -gt 0 ] || [ "$REFACTOR_COUNT" -gt 0 ] || [ "$PERF_COUNT" -gt 0 ]; then
    BUMP="patch"
fi

# Calculate new version
MAJOR=$(echo "$CURRENT_VERSION" | cut -d. -f1)
MINOR=$(echo "$CURRENT_VERSION" | cut -d. -f2)
PATCH=$(echo "$CURRENT_VERSION" | cut -d. -f3)

case $BUMP in
    minor)
        NEW_VERSION="$MAJOR.$((MINOR + 1)).0"
        ;;
    patch)
        NEW_VERSION="$MAJOR.$MINOR.$((PATCH + 1))"
        ;;
    none)
        NEW_VERSION="$CURRENT_VERSION"
        ;;
esac

# Display analysis
echo "┌─────────────────────────────────────────────────┐"
echo "│ 📊 AUTO-VERSION ANALYSIS                        │"
echo "├─────────────────────────────────────────────────┤"
echo "│ Current version: $CURRENT_VERSION                         │"
echo "│ Commits since $CURRENT_TAG: $TOTAL                         │"
echo "├─────────────────────────────────────────────────┤"
echo "│ BUMP-TRIGGERING:                                │"
[ "$FEAT_COUNT" -gt 0 ] &&     echo "│   feat:     $FEAT_COUNT  → MINOR                         │"
[ "$FIX_COUNT" -gt 0 ] &&      echo "│   fix:      $FIX_COUNT  → PATCH                         │"
[ "$REFACTOR_COUNT" -gt 0 ] &&  echo "│   refactor: $REFACTOR_COUNT  → PATCH                         │"
[ "$PERF_COUNT" -gt 0 ] &&     echo "│   perf:     $PERF_COUNT  → PATCH                         │"
[ "$BREAKING_COUNT" -gt 0 ] &&  echo "│   BREAKING: $BREAKING_COUNT  → MINOR                         │"
echo "│ NO-BUMP:                                        │"
[ "$CHORE_COUNT" -gt 0 ] &&    echo "│   chore:    $CHORE_COUNT                                  │"
[ "$DOCS_COUNT" -gt 0 ] &&     echo "│   docs:     $DOCS_COUNT                                  │"
[ "$CI_COUNT" -gt 0 ] &&       echo "│   ci:       $CI_COUNT                                  │"
[ "$TEST_COUNT" -gt 0 ] &&     echo "│   test:     $TEST_COUNT                                  │"
echo "├─────────────────────────────────────────────────┤"

if [ "$BUMP" = "none" ]; then
    echo "│ ✅ RESULT: No bump needed (only chore/docs/ci)  │"
    echo "└─────────────────────────────────────────────────┘"
    exit 0
fi

echo "│ 🎯 RESULT: $BUMP bump → $NEW_VERSION                 │"
echo "└─────────────────────────────────────────────────┘"

if [ "$MODE" = "--apply" ]; then
    echo ""
    echo "🚀 Applying version bump..."
    ./scripts/bump-version.sh "$NEW_VERSION"
    
    echo ""
    echo "📦 Creating version commit..."
    git add VERSION Cargo.toml src/lib.rs CHANGELOG.md 2>/dev/null
    git commit -m "chore: bump version to $NEW_VERSION" --no-verify
    
    echo ""
    echo "🏷️  Creating tag v$NEW_VERSION..."
    git tag -a "v$NEW_VERSION" -m "Release v$NEW_VERSION"
    
    echo ""
    echo "✅ Auto-version applied: $CURRENT_VERSION → $NEW_VERSION"
    echo "   Run: git push origin main --tags"
elif [ "$MODE" = "--dry-run" ]; then
    echo ""
    echo "ℹ️  Dry run — no changes made"
    echo "   To apply: ./scripts/auto-version.sh --apply"
    echo "   Or: task auto-version"
fi
