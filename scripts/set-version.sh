#!/usr/bin/env bash
# Sets the crate version in Cargo.toml and Cargo.lock, so `cargo build --locked` still accepts the lockfile.
# The release workflow runs it before compiling, so the binary reports the version it is released under.
# Usage: scripts/set-version.sh <semver>
set -euo pipefail

version="${1:?usage: scripts/set-version.sh <version>}"
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?(\+[0-9A-Za-z.-]+)?$ ]] ||
  { echo "not a semver version: $version" >&2; exit 1; }

cd "$(dirname "$0")/.."

# Rewrites the first `version = ...` line that follows a line equal to `after`, and fails if there is none.
set_version() {
  local file=$1 after=$2
  awk -v v="$version" -v after="$after" '
    armed && !done && /^version = / { $0 = "version = \"" v "\""; done = 1 }
    $0 == after { armed = 1 }
    { print }
    END { exit !done }' "$file" > "$file.tmp" ||
    { rm -f "$file.tmp"; echo "no version line after '$after' in $file" >&2; exit 1; }
  mv "$file.tmp" "$file"
}

set_version Cargo.toml '[package]'
set_version Cargo.lock 'name = "termrevive"'
echo "termrevive version set to $version"
