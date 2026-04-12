#!/bin/bash
# bump-version.sh - Bumps version across all project files
# Usage: ./scripts/bump-version.sh <new_version>
#
# Updates: VERSION, Cargo.toml, src/lib.rs, CHANGELOG.md

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

if [ "$CURRENT_VERSION" = "$NEW_VERSION" ]; then
    echo "⚠️  Version is already $NEW_VERSION"
    exit 0
fi

# 1. Update VERSION file
echo "$NEW_VERSION" > VERSION
echo "   ✅ VERSION updated"

# 2. Update Cargo.toml
sed -i "s/^version = \".*\"/version = \"$NEW_VERSION\"/" Cargo.toml
echo "   ✅ Cargo.toml updated"

# 3. Update src/lib.rs VERSION const
if [ -f src/lib.rs ]; then
    sed -i "s/pub const VERSION: \&str = \".*\";/pub const VERSION: \&str = \"$NEW_VERSION\";/" src/lib.rs
    echo "   ✅ src/lib.rs updated"
fi

# 4. Update CHANGELOG.md — move [Unreleased] to [new_version]
if [ -f CHANGELOG.md ]; then
    TODAY=$(date +%Y-%m-%d)
    # Replace [Unreleased] header with the new version + date, and add new empty Unreleased
    sed -i "s/## \[Unreleased\]/## [Unreleased]\n\n### Added\n\n### Changed\n\n### Fixed\n\n---\n\n## [$NEW_VERSION] - $TODAY/" CHANGELOG.md
    echo "   ✅ CHANGELOG.md updated ([$NEW_VERSION] - $TODAY)"
fi

# 5. Verify all 3 sources match
V1=$(cat VERSION)
V2=$(grep '^version' Cargo.toml | sed 's/version = "\(.*\)"/\1/')
V3=$(grep 'pub const VERSION' src/lib.rs 2>/dev/null | sed 's/.*"\(.*\)".*/\1/')

echo ""
if [ "$V1" = "$NEW_VERSION" ] && [ "$V2" = "$NEW_VERSION" ] && [ "$V3" = "$NEW_VERSION" ]; then
    echo "   ✅ All 3 sources in sync: $NEW_VERSION"
else
    echo "   ⚠️  Version mismatch detected!"
    echo "      VERSION file: $V1"
    echo "      Cargo.toml:   $V2"
    echo "      src/lib.rs:   $V3"
    exit 1
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
echo "✅ Version bumped from $CURRENT_VERSION → $NEW_VERSION successfully!"