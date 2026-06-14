#!/usr/bin/env bash
# backfill-changelog.sh — One-time maintenance: reconcile CHANGELOG.md with GitHub Releases.
#
# Issue #44: CHANGELOG.md historically accumulated empty "### Added/Changed/Fixed"
# placeholders while the real content lived only in GitHub Releases. This script:
#   1. Fetches every GitHub Release body (the source of truth).
#   2. Fills each empty CHANGELOG version section from its matching release.
#   3. Deduplicates repeated version headers (keeps the populated one).
#   4. Reorders sections: [Unreleased] first, then versions in descending semver.
#   5. Marks versions with no release (or an empty release body) with a placeholder.
#
# Idempotent-ish: re-running only refills sections that are still empty.
# A backup is written to CHANGELOG.md.bak before any change.
#
# Usage:   scripts/backfill-changelog.sh
# Env:     REPO (default enerBydev/nexus-ai-gateway), CHANGELOG_FILE (default CHANGELOG.md)
set -euo pipefail

REPO="${REPO:-enerBydev/nexus-ai-gateway}"
CHANGELOG="${CHANGELOG_FILE:-CHANGELOG.md}"
[ -f "$CHANGELOG" ] || { echo "backfill: $CHANGELOG not found" >&2; exit 1; }

RELEASES_JSON="$(mktemp)"
trap 'rm -f "$RELEASES_JSON"' EXIT

echo "backfill: fetching releases from ${REPO} ..."
gh api "repos/${REPO}/releases" --paginate \
     --jq '.[] | {tag: .tag_name, body: .body}' \
  | python3 -c "import sys,json; d=[json.loads(l) for l in sys.stdin if l.strip()]; json.dump({x['tag']:x['body'] for x in d}, open('${RELEASES_JSON}','w'))"

cp "$CHANGELOG" "${CHANGELOG}.bak"
echo "backfill: backup written to ${CHANGELOG}.bak"

CHANGELOG_FILE="$CHANGELOG" RELEASES_JSON="$RELEASES_JSON" python3 - <<'PY'
import re, json, os

CHANGELOG = os.environ["CHANGELOG_FILE"]
releases = json.load(open(os.environ["RELEASES_JSON"], encoding="utf-8"))

txt = open(CHANGELOG, encoding="utf-8").read()
m = re.search(r'(?m)^## \[', txt)
preamble = txt[:m.start()].rstrip()
rest = txt[m.start():]

parts = re.split(r'(?m)^(## \[[^\]]+\][^\n]*)$', rest)
blocks = []
i = 1
while i < len(parts):
    header = parts[i].strip()
    body = parts[i + 1] if i + 1 < len(parts) else ""
    ver = re.match(r'## \[([^\]]+)\]', header).group(1)
    blocks.append([ver, header, body])
    i += 2

def is_empty(body):
    return not re.search(r'(?m)^- ', body)

def date_of(header):
    md = re.search(r'(\d{4}-\d{2}-\d{2})', header)
    return md.group(1) if md else None

best = {}
for ver, header, body in blocks:
    if ver not in best or (is_empty(best[ver][2]) and not is_empty(body)):
        best[ver] = [ver, header, body]

filled, kept, no_source = [], [], []
for ver, blk in best.items():
    if ver == "Unreleased":
        continue
    if is_empty(blk[2]):
        rbody = releases.get("v" + ver) or ""
        if rbody.strip():
            date = date_of(blk[1])
            blk[1] = f"## [{ver}] - {date}" if date else f"## [{ver}]"
            blk[2] = rbody.strip()
            filled.append(ver)
        else:
            no_source.append(ver)
            blk[2] = "_No release notes recorded._"
    else:
        kept.append(ver)

def semver(v):
    try:
        return tuple(int(x) for x in v.split("."))
    except Exception:
        return (0, 0, 0)

versions = sorted([v for v in best if v != "Unreleased"], key=semver, reverse=True)
ordered = (["Unreleased"] if "Unreleased" in best else []) + versions

out = [preamble]
for ver in ordered:
    header = best[ver][1].strip()
    body = re.sub(r'\n*-{3,}\s*$', '', best[ver][2].strip()).strip()
    out += ["", header, "", body, "", "---"]
open(CHANGELOG, "w", encoding="utf-8").write("\n".join(out).rstrip() + "\n")

print(f"backfill: filled {len(filled)} from releases, {len(no_source)} placeholders, {len(kept)} already populated")
if no_source:
    print(f"backfill: no release body for: {', '.join(no_source)}")
PY

echo "backfill: done -> ${CHANGELOG}"
