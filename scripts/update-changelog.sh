#!/usr/bin/env bash
# update-changelog.sh — Insert a new released version section into CHANGELOG.md,
# populated from conventional commits via generate-changelog-entries.sh (Issue #44).
#
# Shared single-source-of-truth used by both bump-version.sh (manual) and
# auto-version.yml (CI) so CHANGELOG.md is populated identically to GitHub Releases.
#
# Usage: update-changelog.sh <new_version> [from_ref]
#   <new_version>  e.g. 0.20.0  (no leading 'v')
#   [from_ref]     baseline ref for the commit range (default: most recent tag)
#
# Inserts "## [<new_version>] - <today>" with generated entries immediately after
# the "## [Unreleased]" block. Idempotent: skips if the version is already present.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
NEW_VERSION="${1:?Usage: $0 <new_version> [from_ref]}"
FROM_REF="${2:-$(git tag --sort=-version:refname | head -1)}"
CHANGELOG="${CHANGELOG_FILE:-CHANGELOG.md}"
TODAY="$(date +%Y-%m-%d)"

[ -f "$CHANGELOG" ] || { echo "update-changelog: $CHANGELOG not found — skipping"; exit 0; }

# Idempotency: do nothing if this version is already documented.
ver_re="${NEW_VERSION//./\\.}"
if grep -qE "^## \[${ver_re}\]" "$CHANGELOG"; then
  echo "update-changelog: [$NEW_VERSION] already present — skipping"
  exit 0
fi

ENTRIES="$(mktemp)"
TMP="$(mktemp)"
trap 'rm -f "$ENTRIES" "$TMP"' EXIT

if [ -n "$FROM_REF" ]; then
  # Fail loudly on an invalid baseline ref instead of silently using the fallback.
  if ! git rev-parse --verify --quiet "${FROM_REF}^{commit}" >/dev/null; then
    echo "update-changelog: invalid from_ref '${FROM_REF}'" >&2
    exit 1
  fi
  "$HERE/generate-changelog-entries.sh" "$FROM_REF" HEAD "$ENTRIES"
fi
# Fallback when there are no classifiable commits.
if [ ! -s "$ENTRIES" ]; then
  printf '### Changed\n- Maintenance release\n\n' > "$ENTRIES"
fi

# Insert the new version block right after the "## [Unreleased]" block's "---".
awk -v ver="$NEW_VERSION" -v date="$TODAY" -v entries="$ENTRIES" '
  /^## \[Unreleased\]/ { seen=1; print; next }
  (seen && !done && /^---[[:space:]]*$/) {
    print                              # the "---" that closes [Unreleased]
    print ""
    print "## [" ver "] - " date
    print ""
    while ((getline l < entries) > 0) print l
    close(entries)
    print "---"
    done=1
    next
  }
  { print }
  END { if (!done) exit 3 }
' "$CHANGELOG" > "$TMP" || {
  echo "update-changelog: anchor '## [Unreleased]' + '---' not found — CHANGELOG unchanged" >&2
  exit 1
}

mv "$TMP" "$CHANGELOG"
echo "update-changelog: inserted [$NEW_VERSION] - $TODAY (from ${FROM_REF:-<none>})"
