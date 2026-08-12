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

CURRENT_NAME="${1:-$(git branch --show-current)}"
CORE_NAME="${2:-core}"
MASTER_NAME="${3:-master}"
STUDIO_NAME="${4:-studio}"
FORGE_NAME="${5:-forge}"

CORE_REF="$(resolve_ref "$CORE_NAME")"
MASTER_REF="$(resolve_ref "$MASTER_NAME")"
STUDIO_REF="$(resolve_ref "$STUDIO_NAME")"
FORGE_REF="$(resolve_ref "$FORGE_NAME")"

check_core() {
  local product_root
  for product_root in studio forge platform; do
    local files
    files="$(git ls-tree -r --name-only "$CORE_REF" -- "$product_root/")"
    if [[ -n "$files" ]]; then
      echo "$CORE_NAME must not contain $product_root/** files:" >&2
      echo "$files" >&2
      return 1
    fi
  done
}

check_product() {
  local product_name="$1"
  local product_ref="$2"
  local owned_root="$3"
  local sibling_root="$4"

  if ! git merge-base --is-ancestor "$CORE_REF" "$product_ref"; then
    echo "$product_name does not contain the latest $CORE_NAME history" >&2
    echo "merge $CORE_NAME into $product_name before pushing $product_name" >&2
    return 1
  fi

  local forbidden_root
  for forbidden_root in "$sibling_root" platform; do
    local forbidden_files
    forbidden_files="$(git ls-tree -r --name-only "$product_ref" -- "$forbidden_root/")"
    if [[ -n "$forbidden_files" ]]; then
      echo "$product_name must not contain $forbidden_root/** files:" >&2
      echo "$forbidden_files" >&2
      return 1
    fi
  done

  if ! git diff --quiet "$CORE_REF" "$product_ref" -- . ":(exclude)$owned_root/**"; then
    echo "$product_name contains changes outside $owned_root/** that are absent from $CORE_NAME:" >&2
    git diff --name-status "$CORE_REF" "$product_ref" -- . ":(exclude)$owned_root/**" >&2
    return 1
  fi

  # Product ownership predates this policy. Studio in particular was migrated
  # from the retired platform/** tree, so audit product-only commits from the
  # first owned-path commit forward and allow that historical path during the
  # migration. The final-tree checks above still reject platform/** today.
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

    if ((parent_count > 1)); then
      local core_merge=0
      local parent
      for parent in "${commit_and_parents[@]:2}"; do
        if git merge-base --is-ancestor "$parent" "$CORE_REF"; then
          core_merge=1
          break
        fi
      done
      if ((core_merge)); then
        continue
      fi
    fi

    local changed_paths
    if ((parent_count > 0)); then
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
        platform/*) [[ "$owned_root" == studio ]] || invalid_paths+="${path}"$'\n' ;;
        *) invalid_paths+="${path}"$'\n' ;;
      esac
    done <<<"$changed_paths"
    if [[ -n "$invalid_paths" ]]; then
      echo "$product_name-only commit $commit modifies paths outside $owned_root/**:" >&2
      echo "$invalid_paths" >&2
      invalid_commits=1
    fi
  done < <(git rev-list "$product_ref" --not "$CORE_REF")

  if ((invalid_commits)); then
    echo "make Core changes on $CORE_NAME, then merge $CORE_NAME into $product_name" >&2
    return 1
  fi
}

check_master() {
  local branch_name
  local branch_ref
  for branch_name in "$CORE_NAME" "$STUDIO_NAME" "$FORGE_NAME"; do
    branch_ref="$(resolve_ref "$branch_name")"
    if ! git merge-base --is-ancestor "$branch_ref" "$MASTER_REF"; then
      echo "$MASTER_NAME does not contain the latest $branch_name history" >&2
      echo "merge $branch_name into $MASTER_NAME before pushing $MASTER_NAME" >&2
      return 1
    fi
  done

  if ! git diff --quiet "$CORE_REF" "$MASTER_REF" -- . \
    ':(exclude)studio/**' ':(exclude)forge/**'; then
    echo "$MASTER_NAME has Core changes that are absent from $CORE_NAME:" >&2
    git diff --name-status "$CORE_REF" "$MASTER_REF" -- . \
      ':(exclude)studio/**' ':(exclude)forge/**' >&2
    return 1
  fi
  if ! git diff --quiet "$STUDIO_REF" "$MASTER_REF" -- studio/; then
    echo "$MASTER_NAME studio/** does not match $STUDIO_NAME" >&2
    git diff --name-status "$STUDIO_REF" "$MASTER_REF" -- studio/ >&2
    return 1
  fi
  if ! git diff --quiet "$FORGE_REF" "$MASTER_REF" -- forge/; then
    echo "$MASTER_NAME forge/** does not match $FORGE_NAME" >&2
    git diff --name-status "$FORGE_REF" "$MASTER_REF" -- forge/ >&2
    return 1
  fi

  local invalid_commits=0
  local commit
  while IFS= read -r commit; do
    [[ -n "$commit" ]] || continue
    local parent_count
    parent_count=$(($(git rev-list --parents -n 1 "$commit" | wc -w) - 1))
    if ((parent_count < 2)); then
      echo "$MASTER_NAME-only commit $commit is not an integration merge" >&2
      invalid_commits=1
    fi
  done < <(git rev-list "$MASTER_REF" --not "$CORE_REF" "$STUDIO_REF" "$FORGE_REF")
  if ((invalid_commits)); then
    echo "commit changes on their owning branch and merge that branch into $MASTER_NAME" >&2
    return 1
  fi
}

check_core
case "$CURRENT_NAME" in
  core) ;;
  studio) check_product "$STUDIO_NAME" "$STUDIO_REF" studio forge ;;
  forge) check_product "$FORGE_NAME" "$FORGE_REF" forge studio ;;
  master)
    check_product "$STUDIO_NAME" "$STUDIO_REF" studio forge
    check_product "$FORGE_NAME" "$FORGE_REF" forge studio
    check_master
    ;;
  *)
    check_product "$STUDIO_NAME" "$STUDIO_REF" studio forge
    check_product "$FORGE_NAME" "$FORGE_REF" forge studio
    check_master
    ;;
esac

echo "$CORE_NAME/$STUDIO_NAME/$FORGE_NAME -> $MASTER_NAME boundary check passed"
