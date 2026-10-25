#!/usr/bin/env bash
# Commits the version bump (Cargo.toml and Cargo.lock) to main through the GraphQL createCommitOnBranch
# mutation, so GitHub signs the commit (it shows as Verified), then moves the local checkout onto it.
# semantic-release tags whatever HEAD is after its prepare step, so the tag lands on this commit.
# Fails, without committing, if main is no longer at the local HEAD.
# Usage: scripts/release-commit.sh <version>   (needs GITHUB_TOKEN or GH_TOKEN, and GITHUB_REPOSITORY)
set -euo pipefail

version="${1:?usage: scripts/release-commit.sh <version>}"
: "${GITHUB_TOKEN:-${GH_TOKEN:?}}" "${GITHUB_REPOSITORY:?}"

cd "$(dirname "$0")/.."
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

base64 < Cargo.toml | tr -d '\n' > "$tmp/toml.b64"
base64 < Cargo.lock | tr -d '\n' > "$tmp/lock.b64"

jq -n \
  --arg repo "$GITHUB_REPOSITORY" \
  --arg head "$(git rev-parse HEAD)" \
  --arg headline "chore(release): v$version" \
  --rawfile toml "$tmp/toml.b64" \
  --rawfile lock "$tmp/lock.b64" \
  '{
    query: "mutation($input: CreateCommitOnBranchInput!) { createCommitOnBranch(input: $input) { commit { oid url } } }",
    variables: { input: {
      branch: { repositoryNameWithOwner: $repo, branchName: "main" },
      expectedHeadOid: $head,
      message: { headline: $headline },
      fileChanges: { additions: [
        { path: "Cargo.toml", contents: $toml },
        { path: "Cargo.lock", contents: $lock }
      ] }
    } }
  }' > "$tmp/body.json"

gh api graphql --input "$tmp/body.json" > "$tmp/response.json"
oid=$(jq -r '.data.createCommitOnBranch.commit.oid // empty' "$tmp/response.json")
[[ -n "$oid" ]] || { echo "createCommitOnBranch failed:" >&2; cat "$tmp/response.json" >&2; exit 1; }
echo "committed $(jq -r '.data.createCommitOnBranch.commit.url' "$tmp/response.json")"

git fetch --quiet origin main
git reset --hard "$oid"
