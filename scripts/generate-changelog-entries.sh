#!/usr/bin/env bash
# generate-changelog-entries.sh — Generate CHANGELOG content from conventional commits.
#
# Issue #44: single-source-of-truth for changelog/release-note generation, shared by
# auto-version.yml, bump-version.sh and the backfill script so the same classification
# logic produces both CHANGELOG.md entries and GitHub Release notes.
#
# Usage:   generate-changelog-entries.sh <from_ref> [to_ref] [out_file]
#   <from_ref>   git ref (tag/sha) to start from (exclusive)
#   [to_ref]     git ref to end at (default: HEAD)
#   [out_file]   output file (default: stdout)
#
# Output: Keep-a-Changelog sections — ### Added / ### Changed / ### Fixed / ### Security
# Empty sections are omitted. Prints nothing if there are no relevant commits.
set -euo pipefail

FROM_REF="${1:?Usage: $0 <from_ref> [to_ref] [out_file]}"
TO_REF="${2:-HEAD}"
OUT="${3:-/dev/stdout}"

RANGE="${FROM_REF}..${TO_REF}"

# Automated version-bump commits are noise, not user-facing changes — exclude them.
EXCLUDE_RE='^chore(\([^)]*\))?: bump version to'

# Commit subjects in RANGE matching an (extended) regex, minus the excluded noise.
# `|| true` keeps `set -e` happy when grep finds no matches.
subjects() {
  git log "$RANGE" --no-merges --format='%s' | grep -E "$1" | grep -vE "$EXCLUDE_RE" || true
}

# Strip the conventional-commit prefix: "type(scope): msg" / "type!: msg" -> "msg"
strip_prefix() {
  sed -E 's/^[a-z]+(\([^)]*\))?!?:[[:space:]]*//'
}

# Emit a "### <title>" section from a newline-separated list of commit subjects.
# No-op when the list is empty.
emit_section() {
  local title="$1" raw="$2" line first rest
  [ -z "$raw" ] && return 0
  printf '### %s\n' "$title"
  printf '%s\n' "$raw" | strip_prefix | while IFS= read -r line; do
    [ -z "$line" ] && continue
    first=$(printf '%s' "${line:0:1}" | tr '[:lower:]' '[:upper:]')
    rest="${line:1}"
    printf -- '- %s%s\n' "$first" "$rest"
  done
  printf '\n'
}

{
  emit_section "Added"    "$(subjects '^feat')"
  emit_section "Changed"  "$(subjects '^(refactor|perf|chore|ci|docs|style|build)')"
  emit_section "Fixed"    "$(subjects '^fix')"
  emit_section "Security" "$(git log "$RANGE" --no-merges --format='%s' | grep -iE 'security|audit|vulnerabilit' | grep -vE "$EXCLUDE_RE" || true)"
} > "$OUT"
