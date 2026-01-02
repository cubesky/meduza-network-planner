#!/bin/bash
# Quick check for s6-services empty files git status

echo "=== s6-services Empty Files Status ===" >&2
echo "" >&2

cd "$(dirname "$0")/.."

# Count total empty files
total=$(find s6-services -type f -empty 2>/dev/null | wc -l)
echo "Total empty files in s6-services: $total" >&2

# Count tracked empty files
tracked=0
while IFS= read -r file; do
    if [ -f "$file" ] && [ ! -s "$file" ]; then
        tracked=$((tracked + 1))
    fi
done < <(git ls-files s6-services)

echo "Tracked by git: $tracked" >&2

if [ "$total" -eq "$tracked" ]; then
    echo "✅ All empty files are tracked" >&2
else
    echo "⚠️  WARNING: $((total - tracked)) empty files are NOT tracked!" >&2
    echo "" >&2
    echo "Untracked files:" >&2
    find s6-services -type f -empty 2>/dev/null | while read file; do
        if ! git ls-files --error-unmatch "$file" >/dev/null 2>&1; then
            echo "  - $file" >&2
        fi
    done
    echo "" >&2
    echo "Run: git add s6-services/" >&2
fi

echo "" >&2

# Check for staged deletions of important files
echo "Checking for staged deletions of important files..." >&2
# Exclude watcher-managed services that were intentionally removed from user bundle
deleted=$(git diff --cached --name-only --diff-filter=D | grep "^s6-services/" | grep -E "(dependencies\.d/|contents\.d/)" | grep -vE "user/contents\.d/(dnsmasq|easytier|mihomo|mosdns|tinc)$")
if [ -n "$deleted" ]; then
    echo "⚠️  WARNING: Important files are staged for deletion:" >&2
    echo "$deleted" | sed 's/^/  - /' >&2
else
    echo "✅ No important files staged for deletion" >&2
fi
