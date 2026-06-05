#!/bin/bash
# scripts/sanitize-unicode.sh
# Replaces Unicode characters in Rust source files with ASCII equivalents.
# This eliminates Edit tool failures caused by CC Unicode normalization bugs (C1/C2).
#
# Usage:
#   ./scripts/sanitize-unicode.sh           # Replace in-place
#   ./scripts/sanitize-unicode.sh --dry-run # Preview changes only
#   ./scripts/sanitize-unicode.sh --check   # Exit 1 if any Unicode found (CI mode)

set -euo pipefail

DRY_RUN=false
CHECK_ONLY=false

for arg in "$@"; do
    case "$arg" in
        --dry-run) DRY_RUN=true ;;
        --check)   CHECK_ONLY=true ;;
    esac
done

# Unicode -> ASCII mapping table
# Each entry: "unicode_char" "ascii_replacement"
declare -A UNICODE_MAP=(
    ["→"]="->"
    ["⏱️"]="[TIMEOUT]"
    ["⚠️"]="[WARN]"
    ["✓"]="[OK]"
    ["✗"]="[FAIL]"
    ["🔍"]="[SCAN]"
    ["📐"]="[CALIB]"
    ["🛡️"]="[GUARD]"
    ["📋"]="[TODO]"
    ["🚀"]="[LAUNCH]"
    ["📍"]="[PIN]"
)

CHANGED=0
TOTAL_REPLACEMENTS=0

# Find all .rs source files (exclude test-only files if needed)
while IFS= read -r -d '' file; do
    FILE_HAD_CHANGE=0
    for unicode_char in "${!UNICODE_MAP[@]}"; do
        ascii_replacement="${UNICODE_MAP[$unicode_char]}"

        if grep -q "$unicode_char" "$file" 2>/dev/null; then
            count=$(grep -o "$unicode_char" "$file" 2>/dev/null | wc -l || echo 0)
            TOTAL_REPLACEMENTS=$((TOTAL_REPLACEMENTS + count))

            if [ "$CHECK_ONLY" = true ]; then
                echo "CHECK FAIL: $file contains '$unicode_char' ($count occurrences)"
                CHANGED=1
            elif [ "$DRY_RUN" = true ]; then
                echo "WOULD REPLACE: $file '$unicode_char' -> '$ascii_replacement' ($count occurrences)"
            else
                # Escape special characters for sd
                sd -- "$unicode_char" "$ascii_replacement" "$file"
                echo "REPLACED: $file '$unicode_char' -> '$ascii_replacement' ($count occurrences)"
                FILE_HAD_CHANGE=1
            fi
        fi
    done

    if [ "$FILE_HAD_CHANGE" -eq 1 ] && [ "$CHECK_ONLY" = false ] && [ "$DRY_RUN" = false ]; then
        CHANGED=1
    fi
done < <(find src -name "*.rs" -print0)

if [ "$CHECK_ONLY" = true ]; then
    if [ "$CHANGED" -eq 1 ]; then
        echo ""
        echo "ERROR: Found Unicode characters in source files."
        echo "Run: bash scripts/sanitize-unicode.sh"
        exit 1
    else
        echo "OK: No Unicode characters found in source files."
        exit 0
    fi
elif [ "$DRY_RUN" = true ]; then
    echo ""
    echo "Total replacements that would be made: $TOTAL_REPLACEMENTS"
else
    if [ "$CHANGED" -eq 1 ]; then
        echo ""
        echo "Total replacements made: $TOTAL_REPLACEMENTS"
        echo "Run 'cargo fmt' to fix any formatting changes."
    else
        echo "OK: No Unicode characters found — source is clean."
    fi
fi

exit 0
