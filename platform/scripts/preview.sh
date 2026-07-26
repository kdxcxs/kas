#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
PLATFORM_ROOT="$ROOT/platform"
FRONTEND_ROOT="$PLATFORM_ROOT/frontend"
API_PORT="${KAS_PREVIEW_API_PORT:-3000}"
FRONTEND_PORT="${KAS_PREVIEW_FRONTEND_PORT:-5173}"
PREVIEW_DIR="$(mktemp -d "${TMPDIR:-/tmp}/kas-platform-preview.XXXXXX")"
PACKAGES_DIR="$PREVIEW_DIR/packages"
API_LOG="$PREVIEW_DIR/kas-api.log"
FRONTEND_LOG="$PREVIEW_DIR/frontend.log"
API_PID=""
FRONTEND_PID=""

cleanup() {
  local status=$?
  trap - EXIT INT TERM

  if [[ -n "$FRONTEND_PID" ]] && kill -0 "$FRONTEND_PID" 2>/dev/null; then
    kill "$FRONTEND_PID" 2>/dev/null || true
    wait "$FRONTEND_PID" 2>/dev/null || true
  fi
  if [[ -n "$API_PID" ]] && kill -0 "$API_PID" 2>/dev/null; then
    kill "$API_PID" 2>/dev/null || true
    wait "$API_PID" 2>/dev/null || true
  fi

  if ((status == 0)); then
    echo
    echo "Preview stopped."
  else
    echo
    echo "Preview failed (exit $status)." >&2
    if [[ -s "$API_LOG" ]]; then
      echo "--- API log ---" >&2
      tail -n 120 "$API_LOG" >&2
    fi
    if [[ -s "$FRONTEND_LOG" ]]; then
      echo "--- frontend log ---" >&2
      tail -n 120 "$FRONTEND_LOG" >&2
    fi
  fi

  if [[ "$PREVIEW_DIR" == "${TMPDIR:-/tmp}"/kas-platform-preview.* ]]; then
    rm -rf "$PREVIEW_DIR"
  fi
  exit "$status"
}
trap cleanup EXIT
trap 'exit 0' INT TERM

for command in cargo curl jq npm python3; do
  command -v "$command" >/dev/null || {
    echo "missing required command: $command" >&2
    exit 1
  }
done

CODEX_BIN="${KAS_CODEX_BIN:-$(command -v codex || true)}"
if [[ -z "$CODEX_BIN" || ! -x "$CODEX_BIN" ]]; then
  echo "codex is not executable; install it or set KAS_CODEX_BIN" >&2
  exit 1
fi

python3 - "$API_PORT" "$FRONTEND_PORT" <<'PY'
import socket
import sys

for raw_port in sys.argv[1:]:
    port = int(raw_port)
    sock = socket.socket()
    sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    try:
        sock.bind(("127.0.0.1", port))
    except OSError as error:
        raise SystemExit(f"127.0.0.1:{port} is unavailable: {error}")
    finally:
        sock.close()
PY

cd "$ROOT"
echo "Building KAS and platform packages..."
cargo build --workspace
"$PLATFORM_ROOT/scripts/build-packages.sh" "$PACKAGES_DIR"

if [[ ! -d "$FRONTEND_ROOT/node_modules" ]]; then
  echo "Installing frontend dependencies..."
  npm --prefix "$FRONTEND_ROOT" ci
fi

API="http://127.0.0.1:$API_PORT"
FRONTEND="http://127.0.0.1:$FRONTEND_PORT"
export KAS_DATA_DIR="$PREVIEW_DIR/data"
export KAS_DATABASE="$KAS_DATA_DIR/kas.db"
export KAS_ADDRESS="127.0.0.1:$API_PORT"
export KAS_API_URL="$API"
export KAS_CODEX_BIN="$CODEX_BIN"

SOURCE_CODEX_HOME="${CODEX_HOME:-$HOME/.codex}"
export KAS_CODEX_HOME="$PREVIEW_DIR/codex-home"
mkdir -p "$KAS_CODEX_HOME"
chmod 700 "$KAS_CODEX_HOME"
for entry in auth.json config.toml; do
  if [[ -e "$SOURCE_CODEX_HOME/$entry" ]]; then
    ln -s "$SOURCE_CODEX_HOME/$entry" "$KAS_CODEX_HOME/$entry"
  fi
done

mkdir -p "$KAS_DATA_DIR"
target/debug/kas-migrate
ADMIN_TOKEN="$(target/debug/kas-admin bootstrap preview-admin)"

target/debug/kas-api >"$API_LOG" 2>&1 &
API_PID="$!"

api_ready=false
for _ in $(seq 1 100); do
  if curl --fail --silent "$API/health" >/dev/null; then
    api_ready=true
    break
  fi
  if ! kill -0 "$API_PID" 2>/dev/null; then
    echo "kas-api exited during startup" >&2
    exit 1
  fi
  sleep 0.05
done
if [[ "$api_ready" != true ]]; then
  echo "kas-api did not become ready" >&2
  exit 1
fi

install_package() {
  curl --fail --silent \
    -H "Authorization: Bearer $ADMIN_TOKEN" \
    -H "Content-Type: application/vnd.kas.manifest+tar" \
    --data-binary "@$1" \
    "$API/packages"
}

echo "Installing Thread, Session, Agent, and Message packages..."
install_package "$PACKAGES_DIR/thread.kas" >/dev/null
install_package "$PACKAGES_DIR/session.kas" >/dev/null
install_package "$PACKAGES_DIR/agent.kas" >/dev/null
install_package "$PACKAGES_DIR/message.kas" >/dev/null

wait_for_driver() {
  local path="$1" driver
  for _ in $(seq 1 200); do
    driver="$(
    curl --fail --silent --get \
      -H "Authorization: Bearer $ADMIN_TOKEN" \
        --data-urlencode "path=$path" \
      "$API/resources/by-path"
  )"
    if [[ "$(jq -r '.status.metadata.state' <<<"$driver")" == "running" ]]; then
      return
    fi
    sleep 0.05
  done
  echo "Driver did not become ready: $path" >&2
  return 1
}

wait_for_driver "/manifests/agent/driver"
wait_for_driver "/manifests/message/driver"

AGENT_PAYLOAD="$(
  jq -n --arg cwd "$ROOT" '{
    metadata: {
      path: "/agents/preview",
      manifest: "/manifests/agent",
      name: "Preview Agent"
    },
    spec: {
      instructions: "Be concise and helpful. Work only when the user explicitly asks you to change files.",
      working_directory: $cwd
    }
  }'
)"
curl --fail --silent \
  -H "Authorization: Bearer $ADMIN_TOKEN" \
  -H "Content-Type: application/json" \
  -d "$AGENT_PAYLOAD" \
  "$API/resources" >/dev/null

agent_ready=false
for _ in $(seq 1 400); do
  AGENT="$(
    curl --fail --silent --get \
      -H "Authorization: Bearer $ADMIN_TOKEN" \
      --data-urlencode "path=/agents/preview" \
      "$API/resources/by-path"
  )"
  LINK="$(
    curl --silent --get \
      -H "Authorization: Bearer $ADMIN_TOKEN" \
      --data-urlencode "path=/agents/preview/links/service-account" \
      "$API/resources/by-path"
  )"
  if jq -e '
    .spec == .status.spec
    and .status.metadata.state == "available"
  ' >/dev/null <<<"$AGENT" &&
    jq -e '
      .spec.relation == "/manifests/agent/relations/service-account"
      and .spec.target == "/agents/preview/service-account"
      and .status.metadata.state == "available"
    ' >/dev/null <<<"$LINK"; then
    agent_ready=true
    break
  fi
  sleep 0.05
done
if [[ "$agent_ready" != true ]]; then
  echo "Preview Agent did not finish identity reconciliation" >&2
  exit 1
fi

THREAD_PAYLOAD="$(
  jq -n '{
    metadata: {
      path: "/threads/preview",
      manifest: "/manifests/thread",
      name: "Preview Thread"
    },
    spec: {
      title: "Preview Thread"
    }
  }'
)"
curl --fail --silent \
  -H "Authorization: Bearer $ADMIN_TOKEN" \
  -H "Content-Type: application/json" \
  -d "$THREAD_PAYLOAD" \
  "$API/resources" >/dev/null

create_link() {
  local path="$1" target="$2"
  curl --fail --silent \
    -H "Authorization: Bearer $ADMIN_TOKEN" \
    -H "Content-Type: application/json" \
    -d "$(
      jq -cn \
        --arg path "$path" \
        --arg target "$target" '{
          metadata: {
            path: $path,
            manifest: "/builtin/link",
            name: ($path | split("/") | last)
          },
          spec: {
            relation: "/manifests/thread/relations/participants",
            source: "/threads/preview",
            target: $target,
            metadata: {}
          }
        }'
    )" \
    "$API/resources" >/dev/null
}
create_link "/threads/preview/links/participants/user" "/users/preview-admin"
create_link "/threads/preview/links/participants/agent" "/agents/preview"
for _ in $(seq 1 200); do
  PARTICIPANT_LINK="$(
    curl --fail --silent --get \
      -H "Authorization: Bearer $ADMIN_TOKEN" \
      --data-urlencode "path=/threads/preview/links/participants/agent" \
      "$API/resources/by-path"
  )"
  if [[ "$(jq -r '.status.metadata.state' <<<"$PARTICIPANT_LINK")" == "available" ]]; then
    break
  fi
  sleep 0.05
done
jq -e '.status.metadata.state == "available"' <<<"$PARTICIPANT_LINK" >/dev/null

(
  cd "$FRONTEND_ROOT"
  exec node_modules/.bin/vite \
    --host 127.0.0.1 \
    --port "$FRONTEND_PORT"
) >"$FRONTEND_LOG" 2>&1 &
FRONTEND_PID="$!"

frontend_ready=false
for _ in $(seq 1 100); do
  if curl --fail --silent "$FRONTEND/" >/dev/null; then
    frontend_ready=true
    break
  fi
  if ! kill -0 "$FRONTEND_PID" 2>/dev/null; then
    echo "frontend exited during startup" >&2
    exit 1
  fi
  sleep 0.05
done
if [[ "$frontend_ready" != true ]]; then
  echo "frontend did not become ready" >&2
  exit 1
fi

echo
echo "KAS platform preview is ready"
echo "Frontend:  $FRONTEND/"
echo "API:       $API/"
echo "API base:  /api"
echo "User path: /users/preview-admin"
echo "Token:     $ADMIN_TOKEN"
echo "Agent:     Preview Agent (/agents/preview)"
echo "Logs:      $PREVIEW_DIR"
echo
echo "Press Ctrl-C to stop the preview."

while true; do
  if ! kill -0 "$API_PID" 2>/dev/null; then
    echo "kas-api stopped unexpectedly" >&2
    exit 1
  fi
  if ! kill -0 "$FRONTEND_PID" 2>/dev/null; then
    echo "frontend stopped unexpectedly" >&2
    exit 1
  fi
  sleep 1
done
