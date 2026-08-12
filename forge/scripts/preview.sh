#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
FORGE_ROOT="$ROOT/forge"
API_PORT="${KAS_PREVIEW_API_PORT:-3000}"
PACKAGE_PORT="${KAS_PREVIEW_PACKAGE_PORT:-3004}"
FRONTEND_PORT="${KAS_PREVIEW_FRONTEND_PORT:-5173}"
PREVIEW_DIR="$(mktemp -d "${TMPDIR:-/tmp}/kas-forge-preview.XXXXXX")"
PACKAGES_DIR="$PREVIEW_DIR/packages"
API_LOG="$PREVIEW_DIR/kas-api.log"
FRONTEND_LOG="$PREVIEW_DIR/frontend.log"
API_PID=""
FRONTEND_PID=""

cleanup() {
  local status=$?
  trap - EXIT INT TERM
  for pid in "$FRONTEND_PID" "$API_PID"; do
    if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then
      kill "$pid" 2>/dev/null || true
      wait "$pid" 2>/dev/null || true
    fi
  done
  if ((status != 0)); then
    echo "Forge preview failed (exit $status)." >&2
    [[ -s "$API_LOG" ]] && { echo "--- API log ---" >&2; tail -n 120 "$API_LOG" >&2; }
    [[ -s "$FRONTEND_LOG" ]] && { echo "--- Frontend log ---" >&2; tail -n 80 "$FRONTEND_LOG" >&2; }
  fi
  if [[ "${KAS_KEEP_PREVIEW:-false}" != "true" && "$PREVIEW_DIR" == "${TMPDIR:-/tmp}"/kas-forge-preview.* ]]; then
    rm -rf "$PREVIEW_DIR"
  else
    echo "Preview data retained at $PREVIEW_DIR" >&2
  fi
  exit "$status"
}
trap cleanup EXIT
trap 'exit 0' INT TERM

for command in cargo curl jq npm python3 tar; do
  command -v "$command" >/dev/null || { echo "missing required command: $command" >&2; exit 1; }
done
CODEX_BIN="${KAS_CODEX_BIN:-$(command -v codex || true)}"
if [[ -z "$CODEX_BIN" || ! -x "$CODEX_BIN" ]]; then
  echo "codex is not executable; install it or set KAS_CODEX_BIN" >&2
  exit 1
fi

python3 - "$API_PORT" "$PACKAGE_PORT" "$FRONTEND_PORT" <<'PY'
import socket, sys
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

echo "Building KAS Core and Forge..."
cargo build --manifest-path "$ROOT/Cargo.toml" --workspace
"$FORGE_ROOT/scripts/build-packages.sh" "$PACKAGES_DIR"
if [[ ! -d "$FORGE_ROOT/frontend/node_modules" ]]; then
  npm --prefix "$FORGE_ROOT/frontend" install
fi

API="http://127.0.0.1:$API_PORT"
PACKAGE_API="http://127.0.0.1:$PACKAGE_PORT"
FRONTEND="http://127.0.0.1:$FRONTEND_PORT"
export KAS_DATA_DIR="$PREVIEW_DIR/data"
export KAS_DATABASE="$KAS_DATA_DIR/kas.db"
export KAS_ADDRESS="127.0.0.1:$API_PORT"
export KAS_API_URL="$API"
export KAS_PACKAGE_REQUEST_ADDRESS="127.0.0.1:$PACKAGE_PORT"
export KAS_PACKAGE_REQUEST_API="$PACKAGE_API"
export KAS_FORGE_FRONTEND_PORT="$FRONTEND_PORT"
export KAS_CODEX_BIN="$CODEX_BIN"

SOURCE_CODEX_HOME="${CODEX_HOME:-$HOME/.codex}"
export KAS_CODEX_HOME="$PREVIEW_DIR/codex-home"
mkdir -p "$KAS_CODEX_HOME" "$KAS_DATA_DIR"
chmod 700 "$KAS_CODEX_HOME"
for entry in auth.json config.toml; do
  if [[ -e "$SOURCE_CODEX_HOME/$entry" ]]; then
    ln -s "$SOURCE_CODEX_HOME/$entry" "$KAS_CODEX_HOME/$entry"
  fi
done

"$ROOT/target/debug/kas-migrate"
ADMIN_TOKEN="$("$ROOT/target/debug/kas-admin" bootstrap preview-admin)"
"$ROOT/target/debug/kas-api" >"$API_LOG" 2>&1 &
API_PID="$!"

wait_http() {
  local url="$1" name="$2"
  for _ in $(seq 1 200); do
    if curl --fail --silent "$url" >/dev/null; then return 0; fi
    sleep 0.05
  done
  echo "$name did not become ready" >&2
  return 1
}
wait_http "$API/health" "KAS API"

install_package() {
  local response_file="$PREVIEW_DIR/install-response.json"
  if ! curl --fail-with-body --silent --show-error \
    -H "Authorization: Bearer $ADMIN_TOKEN" \
    -H "Content-Type: application/vnd.kas.manifest+tar" \
    --data-binary "@$1" "$API/packages" >"$response_file"; then
    cat "$response_file" >&2
    return 1
  fi
  cat "$response_file"
}
echo "Installing Forge Packages..."
install_package "$PACKAGES_DIR/package-request.kas" >/dev/null
install_package "$PACKAGES_DIR/agent.kas" >/dev/null

wait_driver() {
  local path="$1" value
  for _ in $(seq 1 300); do
    value="$(curl --silent --get -H "Authorization: Bearer $ADMIN_TOKEN" --data-urlencode "path=$path" "$API/resources/by-path" || true)"
    if [[ "$(jq -r '.status.metadata.state' <<<"$value")" == "running" ]]; then return 0; fi
    sleep 0.05
  done
  echo "Driver did not become ready: $path" >&2
  return 1
}
wait_driver "/packages/forge/package-request/driver"
wait_driver "/packages/forge/agent/driver"
wait_http "$PACKAGE_API/health" "Package Request API"

AGENT_PATH="/packages/forge/agent/agents/preview"
curl --fail-with-body --silent --show-error \
  -H "Authorization: Bearer $ADMIN_TOKEN" \
  -H "Content-Type: application/json" \
  -d "$(jq -cn --arg path "$AGENT_PATH" --arg cwd "$ROOT" '{path:$path,metadata:{manifest:"/packages/forge/agent/manifest",name:"Forge Agent"},spec:{working_directory:$cwd,description:"Builds and evolves KAS engineering capabilities."}}')" \
  "$API/resources" >/dev/null

for _ in $(seq 1 300); do
  agent="$(curl --fail --silent --get -H "Authorization: Bearer $ADMIN_TOKEN" --data-urlencode "path=$AGENT_PATH" "$API/resources/by-path")"
  role_link="$(curl --silent --get -H "Authorization: Bearer $ADMIN_TOKEN" --data-urlencode "path=/packages/forge/agent/links/agents/preview-runtime-role" "$API/resources/by-path")"
  if [[ "$(jq -r '.status.metadata.state' <<<"$agent")" == "available" ]] &&
     [[ "$(jq -r '.status.metadata.state // empty' <<<"$role_link")" == "available" ]]; then
    break
  fi
  sleep 0.05
done
jq -e '.status.metadata.state == "available"' <<<"$agent" >/dev/null
jq -e '.status.metadata.state == "available"' <<<"$role_link" >/dev/null

if [[ "${KAS_FORGE_SEED_REQUEST:-true}" == "true" ]]; then
  AGENT_TOKEN="$(
    curl --fail-with-body --silent --show-error \
      -H "Authorization: Bearer $ADMIN_TOKEN" \
      -H "Content-Type: application/json" \
      -d '{"subject":"/packages/forge/agent/service-accounts/preview"}' \
      "$API/credentials/issue" |
      jq -r '.token'
  )"
  curl --fail-with-body --silent --show-error \
    -X POST \
    -H "Authorization: Bearer $AGENT_TOKEN" \
    -H "Content-Type: application/vnd.kas.manifest+tar" \
    -H "X-KAS-Reason: The Agent needs a minimal greeting Resource type to demonstrate controlled self-extension." \
    --data-binary "@$PACKAGES_DIR/hello.kas" \
    "$PACKAGE_API/package-requests" >/dev/null
fi

KAS_API_URL="$API" KAS_PACKAGE_REQUEST_API="$PACKAGE_API" KAS_FORGE_FRONTEND_PORT="$FRONTEND_PORT" \
  npm --prefix "$FORGE_ROOT/frontend" run dev -- --host 127.0.0.1 --port "$FRONTEND_PORT" >"$FRONTEND_LOG" 2>&1 &
FRONTEND_PID="$!"
wait_http "$FRONTEND/" "Forge frontend"

echo
echo "KAS Forge preview is ready"
echo "Frontend:    $FRONTEND/?token=$ADMIN_TOKEN"
echo "API:         $API/"
echo "Package API: $PACKAGE_API/"
echo "User:        /packages/kas/user/users/preview-admin"
echo "Agent:       $AGENT_PATH"
echo "Database:    $KAS_DATABASE"
echo "Logs:        $PREVIEW_DIR"
echo
echo "Press Ctrl-C to stop the preview."

while true; do
  kill -0 "$API_PID" 2>/dev/null || { echo "kas-api stopped unexpectedly" >&2; exit 1; }
  kill -0 "$FRONTEND_PID" 2>/dev/null || { echo "frontend stopped unexpectedly" >&2; exit 1; }
  sleep 1
done
