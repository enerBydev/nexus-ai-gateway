#!/bin/bash
# setup-hooks.sh — Configure git to use portable hooks from scripts/hooks/
# Run once after cloning: ./scripts/setup-hooks.sh

set -e

HOOKS_DIR="scripts/hooks"

if [ ! -d "$HOOKS_DIR" ]; then
    echo "❌ hooks directory not found: $HOOKS_DIR"
    echo "   Run this from the project root"
    exit 1
fi

# Set core.hooksPath to scripts/hooks/
git config core.hooksPath "$HOOKS_DIR"

echo "✅ Git hooks configured to use $HOOKS_DIR"
echo ""
echo "Active hooks:"
for h in "$HOOKS_DIR"/*; do
    if [ -f "$h" ] && [ -x "$h" ]; then
        echo "   ✅ $(basename $h)"
    fi
done
echo ""
echo "Hooks will be shared via git (no manual .git/hooks/ setup needed)"
