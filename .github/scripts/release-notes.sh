#!/usr/bin/env bash
# Print release notes for $1 (a tag): every commit since the previous tag.
set -euo pipefail

tag="${1:?tag required}"

# The tag immediately below $tag in version order, or empty when $tag is the
# first release.
previous_tag=""
while IFS= read -r candidate; do
    if [ "$candidate" = "$tag" ]; then
        break
    fi
    previous_tag="$candidate"
done < <(git tag --list 'v*' --sort=v:refname)

if [ -n "$previous_tag" ]; then
    range="${previous_tag}..${tag}"
else
    range="$tag"
fi

echo "## Changelog"
echo
if [ -n "$previous_tag" ]; then
    echo "Changes since ${previous_tag}."
    echo
fi
for commit in $(git rev-list --reverse "$range"); do
    short="$(git rev-parse --short "$commit")"
    subject="$(git log -1 --format=%s "$commit")"
    body="$(git log -1 --format=%b "$commit")"
    echo "- ${short} ${subject}"
    if [ -n "$body" ]; then
        while IFS= read -r line; do
            if [ -n "$line" ]; then
                echo "  ${line}"
            fi
        done <<<"$body"
    fi
done
