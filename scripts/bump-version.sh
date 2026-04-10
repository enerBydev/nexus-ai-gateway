#!/bin/bash
# bump-version.sh - Bumps version across all project files
# Usage: ./scripts/bump-version.sh <new_version>
#
# Updates: VERSION, Cargo.toml, src/lib.rs

set -e

if [ $# -ne 1 ]; then
    echo "Usage: $0 <new_version>"
    echo "Example: $0 0.6.0"
    exit 1
fi

NEW_VERSION=$1

# Validate version format (X.Y.Z)
if ! echo "$NEW_VERSION" | grep -qE '^[0-9]+\.[0-9]+\.[0-9]+$'; then
    echo "❌ Invalid version format: $NEW_VERSION"
    echo "   Expected format: X.Y.Z (e.g., 0.6.0)"
    exit 1
fi

echo "📦 Bumping version to $NEW_VERSION"

# Get current version
CURRENT_VERSION=$(cat VERSION 2>/dev/null || grep '^version' Cargo.toml | sed 's/version = "\(.*\)"/\1/')
echo "   Current: $CURRENT_VERSION"

# Update VERSION file
echo "$NEW_VERSION" > VERSION
echo "   ✅ VERSION updated"

# Update Cargo.toml
sed -i "s/^version = \".*\"/version = \"$NEW_VERSION\"/" Cargo.toml
echo "   ✅ Cargo.toml updated"

# Update src/lib.rs if exists
if [ -f src/lib.rs ]; then
    # Escape ampersand for sed
    sed -i "s/pub const VERSION: \&str = \".*\";/pub const VERSION: \&str = \"$NEW_VERSION\";/" src/lib.rs
    echo "   ✅ src/lib.rs updated"
fi

# Show diff
echo ""
echo "📝 Changes:"
git diff --stat 2>/dev/null || echo "   (no git diff available)"

# Show next steps
echo ""
echo "🚀 Next steps:"
echo "   1. Review changes: git diff"
echo "   2. Commit: git commit -am 'chore: bump version to $NEW_VERSION'"
echo "   3. Push: git push"
echo "   4. Create release: task release"
echo ""
echo "✅ Version bumped to $NEW_VERSION successfully!"