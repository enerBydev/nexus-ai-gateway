#!/bin/bash

# increment-version.sh - Calculates next version (patch/minor/major)
# Usage: ./scripts/increment-version.sh patch|minor|major

set -e

if [ $# -ne 1 ]; then
    echo "Usage: $0 patch|minor|major"
    exit 1
fi

VERSION_TYPE=$1

# Get current version from VERSION file or Cargo.toml
if [ -f "VERSION" ]; then
    CURRENT_VERSION=$(cat VERSION)
else
    CURRENT_VERSION=$(grep '^version' Cargo.toml | sed 's/version = "\(.*\)"/\1/')
fi

# Parse version components
MAJOR=$(echo $CURRENT_VERSION | cut -d. -f1)
MINOR=$(echo $CURRENT_VERSION | cut -d. -f2)
PATCH=$(echo $CURRENT_VERSION | cut -d. -f3)

case $VERSION_TYPE in
    major)
        NEW_VERSION="$((MAJOR + 1)).0.0"
        ;;
    minor)
        NEW_VERSION="$MAJOR.$((MINOR + 1)).0"
        ;;
    patch)
        NEW_VERSION="$MAJOR.$MINOR.$((PATCH + 1))"
        ;;
    *)
        echo "Invalid version type. Use: major, minor, or patch"
        exit 1
        ;;
esac

echo $NEW_VERSION