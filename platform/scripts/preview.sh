#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
PLATFORM_ROOT="$ROOT/platform"
API_PORT="${KAS_PREVIEW_API_PORT:-3000}"
FILE_PORT="${KAS_PREVIEW_FILE_PORT:-3001}"
SKILL_PORT="${KAS_PREVIEW_SKILL_PORT:-3002}"
APPROVAL_PORT="${KAS_PREVIEW_APPROVAL_PORT:-3003}"
FRONTEND_PORT="${KAS_PREVIEW_FRONTEND_PORT:-5173}"
PREVIEW_DIR="$(mktemp -d "${TMPDIR:-/tmp}/kas-platform-preview.XXXXXX")"
PACKAGES_DIR="$PREVIEW_DIR/packages"
API_LOG="$PREVIEW_DIR/kas-api.log"
API_PID=""

cleanup() {
  local status=$?
  trap - EXIT INT TERM

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

python3 - "$API_PORT" "$FILE_PORT" "$SKILL_PORT" "$APPROVAL_PORT" "$FRONTEND_PORT" <<'PY'
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

API="http://127.0.0.1:$API_PORT"
FILE_API="http://127.0.0.1:$FILE_PORT"
SKILL_API="http://127.0.0.1:$SKILL_PORT"
APPROVAL_API="http://127.0.0.1:$APPROVAL_PORT"
FRONTEND="http://127.0.0.1:$FRONTEND_PORT"
export KAS_DATA_DIR="$PREVIEW_DIR/data"
export KAS_DATABASE="$KAS_DATA_DIR/kas.db"
export KAS_ADDRESS="127.0.0.1:$API_PORT"
export KAS_API_URL="$API"
export KAS_FILE_ADDRESS="127.0.0.1:$FILE_PORT"
export KAS_FILE_API="$FILE_API"
export KAS_FILE_API_URL="$FILE_API"
export KAS_SKILL_ADDRESS="127.0.0.1:$SKILL_PORT"
export KAS_SKILL_API="$SKILL_API"
export KAS_SKILL_API_URL="$SKILL_API"
export KAS_APPROVAL_ADDRESS="127.0.0.1:$APPROVAL_PORT"
export KAS_APPROVAL_API="$APPROVAL_API"
export KAS_APPROVAL_API_URL="$APPROVAL_API"
export KAS_FRONTEND_ADDRESS="127.0.0.1:$FRONTEND_PORT"
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
  curl --fail-with-body --silent --show-error \
    -H "Authorization: Bearer $ADMIN_TOKEN" \
    -H "Content-Type: application/vnd.kas.manifest+tar" \
    --data-binary "@$1" \
    "$API/packages"
}

echo "Installing Platform packages..."
install_package "$PACKAGES_DIR/thread.kas" >/dev/null
install_package "$PACKAGES_DIR/session.kas" >/dev/null
install_package "$PACKAGES_DIR/file.kas" >/dev/null
install_package "$PACKAGES_DIR/proxy.kas" >/dev/null
install_package "$PACKAGES_DIR/frontend.kas" >/dev/null
install_package "$PACKAGES_DIR/skill.kas" >/dev/null
install_package "$PACKAGES_DIR/approval-result.kas" >/dev/null
install_package "$PACKAGES_DIR/approval.kas" >/dev/null
install_package "$PACKAGES_DIR/agent.kas" >/dev/null
install_package "$PACKAGES_DIR/message.kas" >/dev/null

create_proxy() {
  local path="$1" name="$2" prefix="$3" upstream="$4"
  curl --fail-with-body --silent --show-error \
    -H "Authorization: Bearer $ADMIN_TOKEN" \
    -H "Content-Type: application/json" \
    -d "$(
      jq -cn \
        --arg path "$path" \
        --arg name "$name" \
        --arg prefix "$prefix" \
        --arg upstream "$upstream" '{
          metadata: {
            path: $path,
            manifest: "/manifests/proxy",
            name: $name
          },
          spec: {
            prefix: $prefix,
            upstream: $upstream,
            strip_prefix: true,
            authorization: "session"
          }
        }'
    )" \
    "$API/resources" >/dev/null
}
create_proxy "/proxies/file" "File API" "/files-api" "$FILE_API"
create_proxy "/proxies/skill" "Skill API" "/skills-api" "$SKILL_API"
create_proxy "/proxies/approval" "Approval API" "/approvals-api" "$APPROVAL_API"

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
wait_for_driver "/manifests/file/driver"
wait_for_driver "/manifests/frontend-plugin/driver"
wait_for_driver "/manifests/skill/driver"
wait_for_driver "/manifests/approval/driver"
wait_for_driver "/manifests/message/driver"
for proxy_path in /proxies/file /proxies/skill /proxies/approval; do
  for _ in $(seq 1 200); do
    proxy="$(
      curl --fail --silent --get \
        -H "Authorization: Bearer $ADMIN_TOKEN" \
        --data-urlencode "path=$proxy_path" \
        "$API/resources/by-path"
    )"
    if [[ "$(jq -r '.status.metadata.state' <<<"$proxy")" == "available" ]]; then
      break
    fi
    sleep 0.05
  done
  jq -e '.status.metadata.state == "available"' <<<"$proxy" >/dev/null
done

file_ready=false
for _ in $(seq 1 100); do
  if curl --fail --silent "$FILE_API/health" >/dev/null; then
    file_ready=true
    break
  fi
  sleep 0.05
done
if [[ "$file_ready" != true ]]; then
  echo "File Driver API did not become ready" >&2
  exit 1
fi

"$PLATFORM_ROOT/scripts/build-workspace-plugin.sh" \
  "$PLATFORM_ROOT/frontend/dist" \
  "$PREVIEW_DIR/workspace.zip"
install_workspace_plugin() {
  local id="$1" label="$2" icon="$3" order="$4"
  KAS_API_URL="$API" \
  KAS_FILE_API_URL="$FILE_API" \
  KAS_TOKEN="$ADMIN_TOKEN" \
    "$PLATFORM_ROOT/scripts/install-frontend-plugin.sh" \
      "$PREVIEW_DIR/workspace.zip" \
      "/frontend-plugins/$id" \
      "$id" \
      "$id.html" \
      "$label" \
      "$icon" \
      "$order" \
      "/$id" >/dev/null
}
install_workspace_plugin threads Threads "#" 10
install_workspace_plugin agents Agents A 20
install_workspace_plugin skills Skills "⌁" 30
install_workspace_plugin approvals Approvals "✓" 40

KAS_API_URL="$API" \
KAS_FILE_API_URL="$FILE_API" \
KAS_TOKEN="$ADMIN_TOKEN" \
  "$PLATFORM_ROOT/scripts/build-frontend-plugin.sh" \
    "$PLATFORM_ROOT/plugins/registry" \
    "$PREVIEW_DIR/registry.zip"

KAS_API_URL="$API" \
KAS_FILE_API_URL="$FILE_API" \
KAS_TOKEN="$ADMIN_TOKEN" \
  "$PLATFORM_ROOT/scripts/install-frontend-plugin.sh" \
    "$PREVIEW_DIR/registry.zip" \
    "/frontend-plugins/registry" \
    "registry" \
    "index.html" \
    "Objects" \
    "◇" \
    "50" \
    "/objects" >/dev/null
for _ in $(seq 1 200); do
  PLUGIN="$(
    curl --fail --silent --get \
      -H "Authorization: Bearer $ADMIN_TOKEN" \
      --data-urlencode "path=/frontend-plugins/registry" \
      "$API/resources/by-path"
  )"
  if [[ "$(jq -r '.status.metadata.state' <<<"$PLUGIN")" == "available" ]]; then
    break
  fi
  sleep 0.05
done
jq -e '.status.metadata.state == "available"' <<<"$PLUGIN" >/dev/null
for plugin_path in \
  /frontend-plugins/threads \
  /frontend-plugins/agents \
  /frontend-plugins/skills \
  /frontend-plugins/approvals; do
  for _ in $(seq 1 200); do
    PLUGIN="$(
      curl --fail --silent --get \
        -H "Authorization: Bearer $ADMIN_TOKEN" \
        --data-urlencode "path=$plugin_path" \
        "$API/resources/by-path"
    )"
    if [[ "$(jq -r '.status.metadata.state' <<<"$PLUGIN")" == "available" ]]; then
      break
    fi
    sleep 0.05
  done
  jq -e '.status.metadata.state == "available"' <<<"$PLUGIN" >/dev/null
done

skill_ready=false
for _ in $(seq 1 100); do
  if curl --fail --silent "$SKILL_API/health" >/dev/null; then
    skill_ready=true
    break
  fi
  sleep 0.05
done
if [[ "$skill_ready" != true ]]; then
  echo "Skill Driver API did not become ready" >&2
  exit 1
fi

approval_ready=false
for _ in $(seq 1 100); do
  if curl --fail --silent "$APPROVAL_API/health" >/dev/null; then
    approval_ready=true
    break
  fi
  sleep 0.05
done
if [[ "$approval_ready" != true ]]; then
  echo "Approval Driver API did not become ready" >&2
  exit 1
fi

AGENT_PAYLOAD="$(
  jq -n --arg cwd "$ROOT" '{
    metadata: {
      path: "/agents/preview",
      manifest: "/manifests/agent",
      name: "Preview Agent"
    },
    spec: {
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

frontend_ready=false
for _ in $(seq 1 100); do
  if curl --fail --silent "$FRONTEND/" >/dev/null; then
    frontend_ready=true
    break
  fi
  if ! kill -0 "$API_PID" 2>/dev/null; then
    echo "kas-api exited while Frontend Driver was starting" >&2
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
echo "File API:  $FILE_API/"
echo "Skill API: $SKILL_API/"
echo "Approval:  $APPROVAL_API/"
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
  sleep 1
done
