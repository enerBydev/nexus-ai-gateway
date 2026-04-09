#!/bin/bash

# bump-version.sh - Bumps version in Cargo.toml and VERSION file
# Usage: ./scripts/bump-version.sh <new_version>

set -e

if [ $# -ne 1 ]; then
    echo "Usage: $0 <new_version>"
    exit 1
fi

NEW_VERSION=$1

echo "Bumping version to $NEW_VERSION"

# Update VERSION file
echo "$NEW_VERSION" > VERSION

# Update Cargo.toml
sed -i "s/^version = \".*\"/version = \"$NEW_VERSION\"/" Cargo.toml

# Also update the binary name in Cargo.toml to match the project name change
sed -i "s/nexus-brain/nexus-ai-gateway/g" Cargo.toml

echo "Version bumped to $NEW_VERSION successfully!"
echo "Updated files:"
echo "  - VERSION: $(cat VERSION)"
echo "  - Cargo.toml: version = $NEW_VERSION"