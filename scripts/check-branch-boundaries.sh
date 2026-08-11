#!/usr/bin/env bash
set -euo pipefail

resolve_ref() {
  local name="$1"
  local candidate
  for candidate in "refs/heads/$name" "refs/remotes/origin/$name" "$name"; do
    if git rev-parse --verify --quiet "$candidate^{commit}" >/dev/null; then
      git rev-parse "$candidate^{commit}"
      return
    fi
  done
  echo "cannot resolve Git ref: $name" >&2
  exit 2
}

BASE_NAME="${1:-master}"
STUDIO_NAME="${2:-studio}"
FORGE_NAME="${3:-forge}"
BASE_REF="$(resolve_ref "$BASE_NAME")"

for product_root in studio forge platform; do
  PRODUCT_FILES="$(git ls-tree -r --name-only "$BASE_REF" -- "$product_root/")"
  if [[ -n "$PRODUCT_FILES" ]]; then
    echo "$BASE_NAME must not contain $product_root/** files:" >&2
    echo "$PRODUCT_FILES" >&2
    exit 1
  fi
done

check_product() {
  local product_name="$1"
  local owned_root="$2"
  local legacy_root="${3:-}"
  local product_ref
  product_ref="$(resolve_ref "$product_name")"

  if ! git merge-base --is-ancestor "$BASE_REF" "$product_ref"; then
    echo "$product_name does not contain the latest $BASE_NAME history" >&2
    echo "merge $BASE_NAME into $product_name before pushing either branch" >&2
    return 1
  fi

  local excludes=(":(exclude)$owned_root/**")
  if [[ -n "$legacy_root" ]]; then
    excludes+=(":(exclude)$legacy_root/**")
  fi
  if ! git diff --quiet "$BASE_REF" "$product_ref" -- . "${excludes[@]}"; then
    echo "$product_name contains changes outside $owned_root/** that are absent from $BASE_NAME:" >&2
    git diff --name-status "$BASE_REF" "$product_ref" -- . "${excludes[@]}" >&2
    return 1
  fi

  if [[ -n "$legacy_root" ]]; then
    local legacy_files
    legacy_files="$(git ls-tree -r --name-only "$product_ref" -- "$legacy_root/")"
    if [[ -n "$legacy_files" ]]; then
      echo "$product_name still contains retired $legacy_root/** files:" >&2
      echo "$legacy_files" >&2
      return 1
    fi
  fi

  local policy_start
  policy_start="$(git rev-list --reverse "$product_ref" -- "$owned_root/" | head -n 1)"
  if [[ -z "$policy_start" ]]; then
    echo "$product_name does not contain its owned $owned_root/** directory" >&2
    return 1
  fi

  local invalid_commits=0
  local commit
  while IFS= read -r commit; do
    [[ -n "$commit" ]] || continue
    if ! git merge-base --is-ancestor "$policy_start" "$commit"; then
      continue
    fi
    read -r -a commit_and_parents <<<"$(git rev-list --parents -n 1 "$commit")"
    local parent_count=$((${#commit_and_parents[@]} - 1))
    local changed_paths
    if ((parent_count > 1)); then
      local base_merge=0
      local parent
      for parent in "${commit_and_parents[@]:2}"; do
        if git merge-base --is-ancestor "$parent" "$BASE_REF"; then
          base_merge=1
          break
        fi
      done
      if ((base_merge)); then
        continue
      fi
      changed_paths="$(git diff --name-only "${commit_and_parents[1]}" "$commit")"
    else
      changed_paths="$(git diff-tree --root --no-commit-id --name-only -r "$commit")"
    fi

    local invalid_paths=""
    local path
    while IFS= read -r path; do
      [[ -n "$path" ]] || continue
      case "$path" in
        "$owned_root"/*) ;;
        "$legacy_root"/*) [[ -n "$legacy_root" ]] || invalid_paths+="${path}"$'\n' ;;
        *) invalid_paths+="${path}"$'\n' ;;
      esac
    done <<<"$changed_paths"
    if [[ -n "$invalid_paths" ]]; then
      echo "$product_name-only commit $commit modifies paths outside $owned_root/**:" >&2
      echo "$invalid_paths" >&2
      invalid_commits=1
    fi
  done < <(git rev-list "$product_ref" --not "$BASE_REF")

  if ((invalid_commits)); then
    echo "make Core changes on $BASE_NAME, then merge $BASE_NAME into $product_name" >&2
    return 1
  fi
}

check_product "$STUDIO_NAME" studio platform
check_product "$FORGE_NAME" forge

echo "$BASE_NAME/$STUDIO_NAME/$FORGE_NAME boundary check passed"
