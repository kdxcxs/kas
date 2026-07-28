#!/usr/bin/env bash
set -euo pipefail

resolve_ref() {
  local name="$1"
  local candidate
  for candidate in "$name" "refs/heads/$name" "refs/remotes/origin/$name"; do
    if git rev-parse --verify --quiet "$candidate^{commit}" >/dev/null; then
      git rev-parse "$candidate^{commit}"
      return
    fi
  done
  echo "cannot resolve Git ref: $name" >&2
  exit 2
}

CORE_REF="$(resolve_ref "${1:-core}")"
MASTER_REF="$(resolve_ref "${2:-master}")"

PLATFORM_FILES="$(git ls-tree -r --name-only "$CORE_REF" -- platform/)"
if [[ -n "$PLATFORM_FILES" ]]; then
  echo "core must not contain platform/** files:" >&2
  echo "$PLATFORM_FILES" >&2
  exit 1
fi

if ! git merge-base --is-ancestor "$CORE_REF" "$MASTER_REF"; then
  echo "master does not contain the latest core history" >&2
  echo "merge core into master before pushing either branch" >&2
  exit 1
fi

if ! git diff --quiet "$CORE_REF" "$MASTER_REF" -- . ':(exclude)platform/**'; then
  echo "master contains changes outside platform/** that are absent from core:" >&2
  git diff --name-status "$CORE_REF" "$MASTER_REF" -- . ':(exclude)platform/**' >&2
  exit 1
fi

echo "core/master boundary check passed"
