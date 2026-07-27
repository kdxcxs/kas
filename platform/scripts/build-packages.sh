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

if [[ ! -d "$PLATFORM_ROOT/frontend/node_modules" ]]; then
  npm --prefix "$PLATFORM_ROOT/frontend" ci
fi
npm --prefix "$PLATFORM_ROOT/frontend" run build

case "$PROFILE" in
  debug)
    cargo build \
      --manifest-path "$PLATFORM_ROOT/Cargo.toml" \
      -p kas-agent-driver \
      -p kas-approval-driver \
      -p kas-file-driver \
      -p kas-frontend-driver \
      -p kas-message-driver \
      -p kas-skill-driver
    ;;
  release)
    cargo build \
      --manifest-path "$PLATFORM_ROOT/Cargo.toml" \
      -p kas-agent-driver \
      -p kas-approval-driver \
      -p kas-file-driver \
      -p kas-frontend-driver \
      -p kas-message-driver \
      -p kas-skill-driver \
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
  "$STAGING_ROOT/approval/driver/bin" \
  "$STAGING_ROOT/approval-result" \
  "$STAGING_ROOT/file/driver/bin" \
  "$STAGING_ROOT/frontend/driver/bin" \
  "$STAGING_ROOT/frontend/driver/web" \
  "$STAGING_ROOT/message/driver/bin" \
  "$STAGING_ROOT/proxy" \
  "$STAGING_ROOT/skill/driver/bin" \
  "$STAGING_ROOT/thread" \
  "$STAGING_ROOT/session"
cp "$PLATFORM_ROOT/packages/agent/manifest.json" "$STAGING_ROOT/agent/manifest.json"
cp -R "$PLATFORM_ROOT/packages/agent/resources" "$STAGING_ROOT/agent/resources"
cp \
  "$TARGET_DIR/$PROFILE/kas-agent-driver" \
  "$STAGING_ROOT/agent/driver/bin/kas-agent-driver"
chmod 755 "$STAGING_ROOT/agent/driver/bin/kas-agent-driver"
cp "$PLATFORM_ROOT/packages/approval/manifest.json" "$STAGING_ROOT/approval/manifest.json"
cp -R "$PLATFORM_ROOT/packages/approval/resources" "$STAGING_ROOT/approval/resources"
cp \
  "$TARGET_DIR/$PROFILE/kas-approval-driver" \
  "$STAGING_ROOT/approval/driver/bin/kas-approval-driver"
chmod 755 "$STAGING_ROOT/approval/driver/bin/kas-approval-driver"
cp "$PLATFORM_ROOT/packages/approval-result/manifest.json" "$STAGING_ROOT/approval-result/manifest.json"
cp "$PLATFORM_ROOT/packages/file/manifest.json" "$STAGING_ROOT/file/manifest.json"
cp -R "$PLATFORM_ROOT/packages/file/resources" "$STAGING_ROOT/file/resources"
cp \
  "$TARGET_DIR/$PROFILE/kas-file-driver" \
  "$STAGING_ROOT/file/driver/bin/kas-file-driver"
chmod 755 "$STAGING_ROOT/file/driver/bin/kas-file-driver"
cp "$PLATFORM_ROOT/packages/frontend/manifest.json" "$STAGING_ROOT/frontend/manifest.json"
cp -R "$PLATFORM_ROOT/packages/frontend/resources" "$STAGING_ROOT/frontend/resources"
cp \
  "$TARGET_DIR/$PROFILE/kas-frontend-driver" \
  "$STAGING_ROOT/frontend/driver/bin/kas-frontend-driver"
chmod 755 "$STAGING_ROOT/frontend/driver/bin/kas-frontend-driver"
cp -R "$PLATFORM_ROOT/frontend/dist/." "$STAGING_ROOT/frontend/driver/web/"
cp "$PLATFORM_ROOT/packages/message/manifest.json" "$STAGING_ROOT/message/manifest.json"
cp -R "$PLATFORM_ROOT/packages/message/resources" "$STAGING_ROOT/message/resources"
cp \
  "$TARGET_DIR/$PROFILE/kas-message-driver" \
  "$STAGING_ROOT/message/driver/bin/kas-message-driver"
chmod 755 "$STAGING_ROOT/message/driver/bin/kas-message-driver"
cp "$PLATFORM_ROOT/packages/proxy/manifest.json" "$STAGING_ROOT/proxy/manifest.json"
cp "$PLATFORM_ROOT/packages/skill/manifest.json" "$STAGING_ROOT/skill/manifest.json"
cp -R "$PLATFORM_ROOT/packages/skill/resources" "$STAGING_ROOT/skill/resources"
cp \
  "$TARGET_DIR/$PROFILE/kas-skill-driver" \
  "$STAGING_ROOT/skill/driver/bin/kas-skill-driver"
chmod 755 "$STAGING_ROOT/skill/driver/bin/kas-skill-driver"
cp "$PLATFORM_ROOT/packages/thread/manifest.json" "$STAGING_ROOT/thread/manifest.json"
cp -R "$PLATFORM_ROOT/packages/thread/resources" "$STAGING_ROOT/thread/resources"
cp "$PLATFORM_ROOT/packages/session/manifest.json" "$STAGING_ROOT/session/manifest.json"
cp -R "$PLATFORM_ROOT/packages/session/resources" "$STAGING_ROOT/session/resources"

COPYFILE_DISABLE=1 tar -C "$STAGING_ROOT/agent" -cf "$OUTPUT_DIR/agent.kas" manifest.json resources driver
COPYFILE_DISABLE=1 tar -C "$STAGING_ROOT/approval" -cf "$OUTPUT_DIR/approval.kas" manifest.json resources driver
COPYFILE_DISABLE=1 tar -C "$STAGING_ROOT/approval-result" -cf "$OUTPUT_DIR/approval-result.kas" manifest.json
COPYFILE_DISABLE=1 tar -C "$STAGING_ROOT/file" -cf "$OUTPUT_DIR/file.kas" manifest.json resources driver
COPYFILE_DISABLE=1 tar -C "$STAGING_ROOT/frontend" -cf "$OUTPUT_DIR/frontend.kas" manifest.json resources driver
COPYFILE_DISABLE=1 tar -C "$STAGING_ROOT/message" -cf "$OUTPUT_DIR/message.kas" manifest.json resources driver
COPYFILE_DISABLE=1 tar -C "$STAGING_ROOT/proxy" -cf "$OUTPUT_DIR/proxy.kas" manifest.json
COPYFILE_DISABLE=1 tar -C "$STAGING_ROOT/skill" -cf "$OUTPUT_DIR/skill.kas" manifest.json resources driver
COPYFILE_DISABLE=1 tar -C "$STAGING_ROOT/thread" -cf "$OUTPUT_DIR/thread.kas" manifest.json resources
COPYFILE_DISABLE=1 tar -C "$STAGING_ROOT/session" -cf "$OUTPUT_DIR/session.kas" manifest.json resources

echo "$OUTPUT_DIR/thread.kas"
echo "$OUTPUT_DIR/session.kas"
echo "$OUTPUT_DIR/file.kas"
echo "$OUTPUT_DIR/frontend.kas"
echo "$OUTPUT_DIR/skill.kas"
echo "$OUTPUT_DIR/message.kas"
echo "$OUTPUT_DIR/proxy.kas"
echo "$OUTPUT_DIR/agent.kas"
echo "$OUTPUT_DIR/approval.kas"
echo "$OUTPUT_DIR/approval-result.kas"
