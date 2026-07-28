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
POLICY_BASELINE="3e09b1a56169a31007a53a3945542d7d77758115"

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

if ! git merge-base --is-ancestor "$POLICY_BASELINE" "$MASTER_REF"; then
  echo "master does not contain the core-boundary policy baseline" >&2
  exit 1
fi

INVALID_COMMITS=0
while IFS= read -r commit; do
  [[ -n "$commit" ]] || continue
  PARENT_COUNT="$(git rev-list --parents -n 1 "$commit" | awk '{print NF - 1}')"
  if ((PARENT_COUNT > 1)); then
    continue
  fi
  INVALID_PATHS="$(
    git diff-tree --root --no-commit-id --name-only -r "$commit" |
      while IFS= read -r path; do
        case "$path" in
          platform/*) ;;
          *) echo "$path" ;;
        esac
      done
  )"
  if [[ -n "$INVALID_PATHS" ]]; then
    echo "master-only commit $commit modifies core-owned paths:" >&2
    echo "$INVALID_PATHS" >&2
    INVALID_COMMITS=1
  fi
done < <(git rev-list "$POLICY_BASELINE..$MASTER_REF" --not "$CORE_REF")

if ((INVALID_COMMITS)); then
  echo "make core changes on core, then merge core into master" >&2
  exit 1
fi

echo "core/master boundary check passed"
