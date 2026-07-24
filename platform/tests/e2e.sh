#!/usr/bin/env bash
set -euo pipefail

VERBOSE=false
while getopts ":v" option; do
  case "$option" in
    v)
      VERBOSE=true
      ;;
    \?)
      echo "usage: $0 [-v]" >&2
      exit 2
      ;;
  esac
done
shift "$((OPTIND - 1))"
if (($# > 0)); then
  echo "usage: $0 [-v]" >&2
  exit 2
fi

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
PLATFORM_ROOT="$ROOT/platform"
E2E_DIR="$(mktemp -d "${TMPDIR:-/tmp}/kas-platform-e2e.XXXXXX")"
API_PID=""
API_LOG="$E2E_DIR/kas-api.log"

cleanup() {
  if [[ -n "$API_PID" ]] && kill -0 "$API_PID" 2>/dev/null; then
    kill "$API_PID" 2>/dev/null || true
    wait "$API_PID" 2>/dev/null || true
  fi
  if [[ "$E2E_DIR" == "${TMPDIR:-/tmp}"/kas-platform-e2e.* ]]; then
    rm -rf "$E2E_DIR"
  fi
}

failed() {
  local line="$1"
  echo "Platform E2E failed at line $line" >&2
  if [[ -f "$API_LOG" ]]; then
    echo "kas-api output:" >&2
    sed -n '1,260p' "$API_LOG" >&2
  fi
}

trap 'failed "$LINENO"' ERR
trap cleanup EXIT

for command in cargo curl jq tar uuidgen python3; do
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

curl() {
  if [[ "$VERBOSE" != true ]]; then
    command curl "$@"
    return
  fi

  local argument
  local displayed
  local response
  local status

  printf '\n>>> Request: curl' >&2
  for argument in "$@"; do
    displayed="$argument"
    if [[ "$displayed" == "Authorization: Bearer "* ]]; then
      displayed="Authorization: Bearer <redacted>"
    fi
    printf ' %q' "$displayed" >&2
  done
  printf '\n' >&2

  set +e
  response="$(command curl "$@")"
  status=$?
  set -e
  printf '<<< Response (curl exit %d):\n' "$status" >&2
  if [[ -z "$response" ]]; then
    printf '<empty body>\n' >&2
  elif jq -e . >/dev/null 2>&1 <<<"$response"; then
    jq . <<<"$response" >&2
  else
    printf '%s\n' "$response" >&2
  fi
  printf '%s\n' "$response"
  return "$status"
}

cd "$ROOT"
cargo build --workspace
"$PLATFORM_ROOT/scripts/build-packages.sh" "$E2E_DIR/packages"

PORT="$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1]); s.close()')"
API="http://127.0.0.1:$PORT"
export KAS_DATA_DIR="$E2E_DIR/data"
export KAS_DATABASE="$KAS_DATA_DIR/kas.db"
export KAS_ADDRESS="127.0.0.1:$PORT"
export KAS_API_URL="$API"
export KAS_CODEX_BIN="$CODEX_BIN"

target/debug/kas-migrate
ADMIN_TOKEN="$(target/debug/kas-admin bootstrap platform-admin)"

target/debug/kas-api >"$API_LOG" 2>&1 &
API_PID="$!"
for _ in $(seq 1 100); do
  if curl --fail --silent "$API/health" >/dev/null; then
    break
  fi
  sleep 0.05
done
curl --fail --silent "$API/health" | jq -e '.ok == true' >/dev/null

install_package() {
  local package="$1"
  curl --fail --silent \
    -H "Authorization: Bearer $ADMIN_TOKEN" \
    -H "Content-Type: application/vnd.kas.manifest+tar" \
    --data-binary "@$package" \
    "$API/manifests"
}

MESSAGE_MANIFEST="$(install_package "$E2E_DIR/packages/message.kas")"
echo "$MESSAGE_MANIFEST" | jq -e '
  .path == "/manifests/message"
  and .driver == null
  and ([.relations[].name] | sort) == ([
    "addressed_to",
    "authored_by",
    "replies_to",
    "thread_root"
  ] | sort)
' >/dev/null

AGENT_MANIFEST="$(install_package "$E2E_DIR/packages/agent.kas")"
echo "$AGENT_MANIFEST" | jq -e '
  .path == "/manifests/agent"
  and .actions[0].path == "/manifests/agent/actions/message"
  and .driver.path == "/manifests/agent/driver"
  and .driver.service_account == "/manifests/agent/service-accounts/driver"
  and .rbac.service_accounts[0].path == "/manifests/agent/service-accounts/driver"
' >/dev/null

DRIVER=""
for _ in $(seq 1 200); do
  DRIVER="$(
    curl --fail --silent --get \
      -H "Authorization: Bearer $ADMIN_TOKEN" \
      --data-urlencode "path=/manifests/agent" \
      "$API/manifests/driver"
  )"
  if [[ "$(echo "$DRIVER" | jq -r '.state')" == "ready" ]]; then
    break
  fi
  sleep 0.05
done
echo "$DRIVER" | jq -e '
  .path == "/manifests/agent/driver"
  and .state == "ready"
  and .metadata.implementation == "codex-cli"
' >/dev/null

mkdir -p "$E2E_DIR/workspace"
AGENT_PATH="/agents/e2e"
AGENT_PAYLOAD="$(
  jq -n --arg path "$AGENT_PATH" --arg cwd "$E2E_DIR/workspace" '{
    path: $path,
    manifest: "/manifests/agent",
    name: "e2e-agent",
    spec: {
      instructions: "Reply with exactly KAS_PLATFORM_E2E_OK and no other text.",
      working_directory: $cwd
    }
  }'
)"
curl --fail --silent \
  -H "Authorization: Bearer $ADMIN_TOKEN" \
  -H "Content-Type: application/json" \
  -d "$AGENT_PAYLOAD" \
  "$API/resources" |
  jq -e '.path == "/agents/e2e"' >/dev/null

MESSAGE_PATH="/messages/e2e-user"
MESSAGE_PAYLOAD="$(
  jq -n --arg message "$MESSAGE_PATH" --arg agent "$AGENT_PATH" '{
    path: $message,
    manifest: "/manifests/message",
    name: "e2e-user-message",
    spec: {
      role: "user",
      body: "hello from platform e2e"
    },
    links: [
      {
        path: ($message + "/links/authored-by"),
        source: {kind: "resource", path: $message},
        relation_path: "/manifests/message/relations/authored-by",
        target: {kind: "user", path: "/users/platform-admin"},
        metadata: {}
      },
      {
        path: ($message + "/links/addressed-to"),
        source: {kind: "resource", path: $message},
        relation_path: "/manifests/message/relations/addressed-to",
        target: {kind: "resource", path: $agent},
        metadata: {}
      },
      {
        path: ($message + "/links/thread-root"),
        source: {kind: "resource", path: $message},
        relation_path: "/manifests/message/relations/thread-root",
        target: {kind: "resource", path: $message},
        metadata: {}
      }
    ]
  }'
)"
curl --fail --silent \
  -H "Authorization: Bearer $ADMIN_TOKEN" \
  -H "Content-Type: application/json" \
  -d "$MESSAGE_PAYLOAD" \
  "$API/resources" |
  jq -e '.spec.body == "hello from platform e2e"' >/dev/null

REQUEST_ID="$(uuidgen | tr '[:upper:]' '[:lower:]')"
RUN_PATH="$AGENT_PATH/runs/$REQUEST_ID"
RUN_PAYLOAD="$(
  jq -n \
    --arg path "$RUN_PATH" \
    --arg request_id "$REQUEST_ID" \
    --arg agent "$AGENT_PATH" \
    --arg message "$MESSAGE_PATH" '{
      path: $path,
      request_id: $request_id,
      resource: $agent,
      action: "/manifests/agent/actions/message",
      input: {message_path: $message}
    }'
)"
curl --fail --silent \
  -H "Authorization: Bearer $ADMIN_TOKEN" \
  -H "Content-Type: application/json" \
  -d "$RUN_PAYLOAD" \
  "$API/runs" |
  jq -e '.status == "queued"' >/dev/null

RUN=""
for _ in $(seq 1 2400); do
  RUN="$(
    curl --fail --silent --get \
      -H "Authorization: Bearer $ADMIN_TOKEN" \
      --data-urlencode "path=$RUN_PATH" \
      "$API/runs/by-path"
  )"
  if [[ "$(echo "$RUN" | jq -r '.status')" =~ ^(succeeded|failed)$ ]]; then
    break
  fi
  sleep 0.05
done
if [[ "$(echo "$RUN" | jq -r '.status')" != "succeeded" ]]; then
  echo "Agent Run failed: $(echo "$RUN" | jq -c '.')" >&2
  false
fi
echo "$RUN" | jq -e '
  .status == "succeeded"
  and .output.reply_message_path != null
  and .driver_generation == 1
' >/dev/null
REPLY_PATH="$(echo "$RUN" | jq -r '.output.reply_message_path')"

REPLY="$(
  curl --fail --silent --get \
    -H "Authorization: Bearer $ADMIN_TOKEN" \
    --data-urlencode "path=$REPLY_PATH" \
    --data-urlencode "include=relations" \
    "$API/resources/by-path"
)"
echo "$REPLY" | jq -e \
  --arg agent "$AGENT_PATH" \
  --arg parent "$MESSAGE_PATH" '
    .spec == {
      role: "assistant",
      body: "KAS_PLATFORM_E2E_OK",
      state: "available"
    }
    and ([.links[].relation_path] | sort) == ([
      "/manifests/message/relations/authored-by",
      "/manifests/message/relations/replies-to",
      "/manifests/message/relations/thread-root",
      "/manifests/system/core/relations/resource-manifest"
    ] | sort)
    and any(.links[];
      .relation_path == "/manifests/message/relations/authored-by"
      and .target.path == $agent
    )
    and any(.links[];
      .relation_path == "/manifests/message/relations/replies-to"
      and .target.path == $parent
    )
    and any(.links[];
      .relation_path == "/manifests/message/relations/thread-root"
      and .target.path == $parent
    )
  ' >/dev/null

DRIVER_URL="$API/drivers/by-path?path=$(jq -rn --arg value "/manifests/agent/driver" '$value | @uri')"
curl --fail --silent --request PATCH \
  -H "Authorization: Bearer $ADMIN_TOKEN" \
  -H "Content-Type: application/json" \
  --data '{"state":"stopping"}' \
  "$DRIVER_URL" >/dev/null

for _ in $(seq 1 100); do
  DRIVER="$(
    curl --fail --silent --get \
      -H "Authorization: Bearer $ADMIN_TOKEN" \
      --data-urlencode "path=/manifests/agent/driver" \
      "$API/drivers/by-path"
  )"
  if [[ "$(echo "$DRIVER" | jq -r '.state')" == "stopped" ]]; then
    break
  fi
  sleep 0.05
done
echo "$DRIVER" | jq -e '.state == "stopped" and .process_id == null' >/dev/null

echo "KAS platform end-to-end test passed"
