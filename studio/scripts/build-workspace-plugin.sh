#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 2 ]]; then
  echo "usage: $0 FRONTEND_DIST OUTPUT_ZIP" >&2
  exit 2
fi

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
FRONTEND_DIST="$1"
OUTPUT_ZIP="$2"
STAGING="$(mktemp -d "${TMPDIR:-/tmp}/kas-workspace-plugin.XXXXXX")"

cleanup() {
  if [[ "$STAGING" == "${TMPDIR:-/tmp}"/kas-workspace-plugin.* ]]; then
    rm -rf "$STAGING"
  fi
}
trap cleanup EXIT

if [[ ! -f "$FRONTEND_DIST/index.html" ]]; then
  echo "frontend dist does not contain index.html: $FRONTEND_DIST" >&2
  exit 1
fi

cp -R "$FRONTEND_DIST/." "$STAGING/"
for view in agents skills approvals threads telegram; do
  cp "$FRONTEND_DIST/index.html" "$STAGING/$view.html"
done

"$ROOT/scripts/build-frontend-plugin.sh" "$STAGING" "$OUTPUT_ZIP"
