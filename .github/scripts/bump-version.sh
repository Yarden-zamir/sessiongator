#!/usr/bin/env bash
# Bump the crate version from conventional commits, then commit and tag it.
#
# Reads the commits since the newest version tag and picks a semver bump:
#   major  a subject with "!:" or a body line starting with BREAKING CHANGE
#   minor  a "feat" subject
#   patch  anything else
# With no commits since the tag nothing is released, which is also what stops
# the release commit this script pushes from triggering another release.
#
# Writes released/tag/version to $GITHUB_OUTPUT when running in Actions.
# Set BUMP_SELF_TEST=1 to load the helpers without running anything.
set -euo pipefail

# Highest semver tag, or empty when the repo has never been tagged.
previous_tag() {
    git tag --list 'v*' --sort=-v:refname | head -n1
}

# Semver level implied by the commits in $1 ("none" when the range is empty).
bump_level_from_range() {
    local range="$1"
    local commits bump="patch" commit subject body
    commits="$(git rev-list --no-merges "$range")"
    if [ -z "$commits" ]; then
        echo "none"
        return 0
    fi
    while IFS= read -r commit; do
        [ -n "$commit" ] || continue
        subject="$(git log -1 --format=%s "$commit")"
        body="$(git log -1 --format=%b "$commit")"
        if [[ "$subject" == *"!:"* ]] || printf '%s\n' "$body" | grep -q '^BREAKING CHANGE'; then
            echo "major"
            return 0
        fi
        if [[ "$subject" == feat* ]]; then
            bump="minor"
        fi
    done <<<"$commits"
    echo "$bump"
}

# True when semver $1 is strictly greater than $2.
version_gt() {
    [ "$1" != "$2" ] &&
        [ "$(printf '%s\n%s\n' "$1" "$2" | sort -V | tail -n1)" = "$1" ]
}

# Apply a bump level to a semver string.
next_version() {
    local current="$1" level="$2"
    local major minor patch
    IFS=. read -r major minor patch <<<"$current"
    case "$level" in
    major)
        major=$((major + 1))
        minor=0
        patch=0
        ;;
    minor)
        minor=$((minor + 1))
        patch=0
        ;;
    patch) patch=$((patch + 1)) ;;
    *)
        echo "$current"
        return 0
        ;;
    esac
    echo "${major}.${minor}.${patch}"
}

# Rewrite the first `version = "..."` line in Cargo.toml.
set_manifest_version() {
    local file="$1" version="$2"
    awk -v version="$version" '
        !done && /^version = "/ { sub(/"[^"]*"/, "\"" version "\""); done = 1 }
        { print }
    ' "$file" >"${file}.tmp"
    mv "${file}.tmp" "$file"
}

# Rewrite the version of one package entry in Cargo.lock, leaving the
# dependency graph untouched so this stays an offline edit.
set_lock_version() {
    local file="$1" package="$2" version="$3"
    [ -f "$file" ] || return 0
    awk -v package="$package" -v version="$version" '
        $0 == "name = \"" package "\"" { in_package = 1 }
        in_package && /^version = "/ {
            sub(/"[^"]*"/, "\"" version "\"")
            in_package = 0
        }
        { print }
    ' "$file" >"${file}.tmp"
    mv "${file}.tmp" "$file"
}

manifest_version() {
    awk -F '"' '/^version =/ { print $2; exit }' "${1:-Cargo.toml}"
}

emit() {
    if [ -n "${GITHUB_OUTPUT:-}" ]; then
        printf '%s\n' "$1" >>"$GITHUB_OUTPUT"
    fi
    printf '%s\n' "$1"
}

if [ -n "${BUMP_SELF_TEST:-}" ]; then
    # Stop here whether the script was sourced (tests) or executed.
    # shellcheck disable=SC2317
    return 0 2>/dev/null || exit 0
fi

package="${1:?package name required}"

tag_before="$(previous_tag)"
if [ -n "$tag_before" ]; then
    range="${tag_before}..HEAD"
else
    range="HEAD"
fi

level="$(bump_level_from_range "$range")"
if [ "$level" = "none" ]; then
    echo "No commits since ${tag_before:-the start of the repository}; nothing to release."
    emit "released=false"
    exit 0
fi

current="$(manifest_version Cargo.toml)"
# A manifest version ahead of the newest tag was bumped by hand and never
# released, so release exactly that instead of bumping past it.
if [ -n "$tag_before" ] && version_gt "$current" "${tag_before#v}"; then
    echo "Cargo.toml is at ${current}, ahead of ${tag_before}; releasing it as-is."
    next="$current"
else
    next="$(next_version "$current" "$level")"
fi
tag="v${next}"

if git rev-parse -q --verify "refs/tags/${tag}" >/dev/null; then
    echo "Tag ${tag} already exists; nothing to release."
    emit "released=false"
    exit 0
fi

echo "Releasing ${package} ${current} -> ${next} (${level} bump over ${tag_before:-nothing})"

set_manifest_version Cargo.toml "$next"
set_lock_version Cargo.lock "$package" "$next"

git config user.name "github-actions[bot]"
git config user.email "github-actions[bot]@users.noreply.github.com"
git add Cargo.toml
[ -f Cargo.lock ] && git add Cargo.lock

# Only when the manifest actually changed: releasing a hand-bumped version
# as-is leaves nothing to commit, and the tag alone is the release.
if git diff --cached --quiet; then
    echo "Manifest already at ${next}; tagging without a release commit."
else
    git commit -m "chore: release ${package} ${next}"
    git push origin HEAD:main
fi
git tag "$tag"
git push origin "$tag"

emit "released=true"
emit "tag=${tag}"
emit "version=${next}"
