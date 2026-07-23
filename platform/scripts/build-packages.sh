#!/usr/bin/env bash
set -euo pipefail

PLATFORM_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUTPUT_DIR="${1:-$PLATFORM_ROOT/dist}"
PROFILE="${KAS_PLATFORM_PROFILE:-debug}"
STAGING_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/kas-platform-packages.XXXXXX")"

cleanup() {
  if [[ "$STAGING_ROOT" == "${TMPDIR:-/tmp}"/kas-platform-packages.* ]]; then
    rm -rf "$STAGING_ROOT"
  fi
}
trap cleanup EXIT

case "$PROFILE" in
  debug)
    cargo build \
      --manifest-path "$PLATFORM_ROOT/Cargo.toml" \
      -p kas-agent-driver
    ;;
  release)
    cargo build \
      --manifest-path "$PLATFORM_ROOT/Cargo.toml" \
      -p kas-agent-driver \
      --release
    ;;
  *)
    echo "KAS_PLATFORM_PROFILE must be debug or release" >&2
    exit 2
    ;;
esac

mkdir -p "$OUTPUT_DIR" "$STAGING_ROOT/agent/driver/bin" "$STAGING_ROOT/message"
cp "$PLATFORM_ROOT/packages/agent/manifest.json" "$STAGING_ROOT/agent/manifest.json"
cp \
  "$PLATFORM_ROOT/target/$PROFILE/kas-agent-driver" \
  "$STAGING_ROOT/agent/driver/bin/kas-agent-driver"
chmod 755 "$STAGING_ROOT/agent/driver/bin/kas-agent-driver"
cp "$PLATFORM_ROOT/packages/message/manifest.json" "$STAGING_ROOT/message/manifest.json"

tar -C "$STAGING_ROOT/agent" -cf "$OUTPUT_DIR/agent.kas" manifest.json driver
tar -C "$STAGING_ROOT/message" -cf "$OUTPUT_DIR/message.kas" manifest.json

echo "$OUTPUT_DIR/message.kas"
echo "$OUTPUT_DIR/agent.kas"
