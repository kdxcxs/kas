#!/usr/bin/env bash
set -euo pipefail

PLATFORM_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUTPUT_DIR="${1:-$PLATFORM_ROOT/dist}"
PROFILE="${KAS_PLATFORM_PROFILE:-debug}"
TARGET_DIR="${CARGO_TARGET_DIR:-$PLATFORM_ROOT/target}"
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
      -p kas-agent-driver \
      -p kas-file-driver \
      -p kas-message-driver
    ;;
  release)
    cargo build \
      --manifest-path "$PLATFORM_ROOT/Cargo.toml" \
      -p kas-agent-driver \
      -p kas-file-driver \
      -p kas-message-driver \
      --release
    ;;
  *)
    echo "KAS_PLATFORM_PROFILE must be debug or release" >&2
    exit 2
    ;;
esac

mkdir -p \
  "$OUTPUT_DIR" \
  "$STAGING_ROOT/agent/driver/bin" \
  "$STAGING_ROOT/file/driver/bin" \
  "$STAGING_ROOT/message/driver/bin" \
  "$STAGING_ROOT/thread" \
  "$STAGING_ROOT/session"
cp "$PLATFORM_ROOT/packages/agent/manifest.json" "$STAGING_ROOT/agent/manifest.json"
cp -R "$PLATFORM_ROOT/packages/agent/resources" "$STAGING_ROOT/agent/resources"
cp \
  "$TARGET_DIR/$PROFILE/kas-agent-driver" \
  "$STAGING_ROOT/agent/driver/bin/kas-agent-driver"
chmod 755 "$STAGING_ROOT/agent/driver/bin/kas-agent-driver"
cp "$PLATFORM_ROOT/packages/file/manifest.json" "$STAGING_ROOT/file/manifest.json"
cp -R "$PLATFORM_ROOT/packages/file/resources" "$STAGING_ROOT/file/resources"
cp \
  "$TARGET_DIR/$PROFILE/kas-file-driver" \
  "$STAGING_ROOT/file/driver/bin/kas-file-driver"
chmod 755 "$STAGING_ROOT/file/driver/bin/kas-file-driver"
cp "$PLATFORM_ROOT/packages/message/manifest.json" "$STAGING_ROOT/message/manifest.json"
cp -R "$PLATFORM_ROOT/packages/message/resources" "$STAGING_ROOT/message/resources"
cp \
  "$TARGET_DIR/$PROFILE/kas-message-driver" \
  "$STAGING_ROOT/message/driver/bin/kas-message-driver"
chmod 755 "$STAGING_ROOT/message/driver/bin/kas-message-driver"
cp "$PLATFORM_ROOT/packages/thread/manifest.json" "$STAGING_ROOT/thread/manifest.json"
cp -R "$PLATFORM_ROOT/packages/thread/resources" "$STAGING_ROOT/thread/resources"
cp "$PLATFORM_ROOT/packages/session/manifest.json" "$STAGING_ROOT/session/manifest.json"
cp -R "$PLATFORM_ROOT/packages/session/resources" "$STAGING_ROOT/session/resources"

COPYFILE_DISABLE=1 tar -C "$STAGING_ROOT/agent" -cf "$OUTPUT_DIR/agent.kas" manifest.json resources driver
COPYFILE_DISABLE=1 tar -C "$STAGING_ROOT/file" -cf "$OUTPUT_DIR/file.kas" manifest.json resources driver
COPYFILE_DISABLE=1 tar -C "$STAGING_ROOT/message" -cf "$OUTPUT_DIR/message.kas" manifest.json resources driver
COPYFILE_DISABLE=1 tar -C "$STAGING_ROOT/thread" -cf "$OUTPUT_DIR/thread.kas" manifest.json resources
COPYFILE_DISABLE=1 tar -C "$STAGING_ROOT/session" -cf "$OUTPUT_DIR/session.kas" manifest.json resources

echo "$OUTPUT_DIR/thread.kas"
echo "$OUTPUT_DIR/session.kas"
echo "$OUTPUT_DIR/file.kas"
echo "$OUTPUT_DIR/message.kas"
echo "$OUTPUT_DIR/agent.kas"
