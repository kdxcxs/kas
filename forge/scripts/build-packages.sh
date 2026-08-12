#!/usr/bin/env bash
set -euo pipefail

FORGE_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUTPUT_DIR="${1:-$FORGE_ROOT/dist}"
PROFILE="${KAS_FORGE_PROFILE:-debug}"
TARGET_DIR="${CARGO_TARGET_DIR:-$FORGE_ROOT/target}"
STAGING_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/kas-forge-packages.XXXXXX")"

cleanup() {
  if [[ "$STAGING_ROOT" == "${TMPDIR:-/tmp}"/kas-forge-packages.* ]]; then
    rm -rf "$STAGING_ROOT"
  fi
}
trap cleanup EXIT

case "$PROFILE" in
  debug) cargo build --manifest-path "$FORGE_ROOT/Cargo.toml" ;;
  release) cargo build --manifest-path "$FORGE_ROOT/Cargo.toml" --release ;;
  *) echo "KAS_FORGE_PROFILE must be debug or release" >&2; exit 2 ;;
esac

mkdir -p \
  "$OUTPUT_DIR" \
  "$STAGING_ROOT/agent/driver/bin" \
  "$STAGING_ROOT/package-request/driver/bin" \
  "$STAGING_ROOT/hello"

cp "$FORGE_ROOT/packages/agent/manifest.json" "$STAGING_ROOT/agent/manifest.json"
cp -R "$FORGE_ROOT/packages/agent/resources" "$STAGING_ROOT/agent/resources"
cp "$TARGET_DIR/$PROFILE/kas-forge-agent-driver" "$STAGING_ROOT/agent/driver/bin/kas-forge-agent-driver"
chmod 755 "$STAGING_ROOT/agent/driver/bin/kas-forge-agent-driver"

cp "$FORGE_ROOT/packages/package-request/manifest.json" "$STAGING_ROOT/package-request/manifest.json"
cp -R "$FORGE_ROOT/packages/package-request/resources" "$STAGING_ROOT/package-request/resources"
cp "$TARGET_DIR/$PROFILE/kas-forge-package-request-driver" "$STAGING_ROOT/package-request/driver/bin/kas-forge-package-request-driver"
chmod 755 "$STAGING_ROOT/package-request/driver/bin/kas-forge-package-request-driver"

cp "$FORGE_ROOT/examples/hello/manifest.json" "$STAGING_ROOT/hello/manifest.json"

COPYFILE_DISABLE=1 tar -C "$STAGING_ROOT/agent" -cf "$OUTPUT_DIR/agent.kas" manifest.json resources driver
COPYFILE_DISABLE=1 tar -C "$STAGING_ROOT/package-request" -cf "$OUTPUT_DIR/package-request.kas" manifest.json resources driver
COPYFILE_DISABLE=1 tar -C "$STAGING_ROOT/hello" -cf "$OUTPUT_DIR/hello.kas" manifest.json

echo "$OUTPUT_DIR/agent.kas"
echo "$OUTPUT_DIR/package-request.kas"
echo "$OUTPUT_DIR/hello.kas"

