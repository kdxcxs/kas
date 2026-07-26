#!/usr/bin/env bash
set -euo pipefail

VERBOSE=false
while getopts ":v" option; do
  case "$option" in
    v) VERBOSE=true ;;
    *)
      echo "usage: $0 [-v]" >&2
      exit 2
      ;;
  esac
done

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
PLATFORM_ROOT="$ROOT/platform"
E2E_DIR="$(mktemp -d "${TMPDIR:-/tmp}/kas-platform-e2e.XXXXXX")"
CORE_TARGET="${KAS_CORE_TARGET_DIR:-$E2E_DIR/core-target}"
PLATFORM_TARGET="${KAS_PLATFORM_TARGET_DIR:-$E2E_DIR/platform-target}"
API_PID=""
API_LOG="$E2E_DIR/kas-api.log"

cleanup() {
  if [[ -n "$API_PID" ]] && kill -0 "$API_PID" 2>/dev/null; then
    kill "$API_PID" 2>/dev/null || true
    wait "$API_PID" 2>/dev/null || true
  fi
  if [[ "${KAS_E2E_KEEP:-false}" != true ]] &&
    [[ "$E2E_DIR" == "${TMPDIR:-/tmp}"/kas-platform-e2e.* ]]; then
    rm -rf "$E2E_DIR"
  fi
}

failed() {
  echo "Platform E2E failed at line $1" >&2
  [[ -f "$API_LOG" ]] && sed -n '1,360p' "$API_LOG" >&2
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

request() {
  local response status
  if [[ "$VERBOSE" == true ]]; then
    printf '\n>>> curl' >&2
    local argument displayed
    for argument in "$@"; do
      displayed="$argument"
      [[ "$displayed" == "Authorization: Bearer "* ]] &&
        displayed="Authorization: Bearer <redacted>"
      printf ' %q' "$displayed" >&2
    done
    printf '\n' >&2
  fi
  set +e
  response="$(command curl "$@")"
  status=$?
  set -e
  if [[ "$VERBOSE" == true ]]; then
    printf '<<<\n' >&2
    jq . <<<"$response" >&2 2>/dev/null || printf '%s\n' "$response" >&2
  fi
  printf '%s\n' "$response"
  return "$status"
}

get_resource() {
  request --fail --silent --get \
    -H "Authorization: Bearer $ADMIN_TOKEN" \
    --data-urlencode "path=$1" \
    "$API/resources/by-path"
}

post_resource() {
  request --fail-with-body --silent --show-error \
    -H "Authorization: Bearer $ADMIN_TOKEN" \
    -H "Content-Type: application/json" \
    -d "$1" \
    "$API/resources"
}

wait_for_state() {
  local path="$1" state="$2" value=""
  for _ in $(seq 1 800); do
    value="$(get_resource "$path" 2>/dev/null || true)"
    if [[ "$(jq -r '.status.metadata.state // empty' <<<"$value")" == "$state" ]]; then
      printf '%s\n' "$value"
      return
    fi
    sleep 0.05
  done
  printf '%s\n' "$value"
  return 1
}

create_link() {
  local path="$1" relation="$2" source="$3" target="$4"
  post_resource "$(
    jq -n \
      --arg path "$path" \
      --arg relation "$relation" \
      --arg source "$source" \
      --arg target "$target" '{
        metadata: {
          path: $path,
          manifest: "/builtin/link",
          name: ($path | split("/") | last)
        },
        spec: {
          relation: $relation,
          source: $source,
          target: $target,
          metadata: {}
        }
      }'
  )" >/dev/null
}

wait_for_run() {
  local path="$1" value="" state=""
  for _ in $(seq 1 3600); do
    value="$(get_resource "$path" 2>/dev/null || true)"
    state="$(jq -r '.status.metadata.state // empty' <<<"$value")"
    if [[ "$state" =~ ^(succeeded|failed|cancelled)$ ]]; then
      printf '%s\n' "$value"
      return
    fi
    sleep 0.05
  done
  printf '%s\n' "$value"
  return 1
}

cd "$ROOT"
CARGO_TARGET_DIR="$CORE_TARGET" cargo build --workspace
CARGO_TARGET_DIR="$PLATFORM_TARGET" "$PLATFORM_ROOT/scripts/build-packages.sh" "$E2E_DIR/packages"

PORT="$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1]); s.close()')"
API="http://127.0.0.1:$PORT"
export KAS_DATA_DIR="$E2E_DIR/data"
export KAS_DATABASE="$KAS_DATA_DIR/kas.db"
export KAS_ADDRESS="127.0.0.1:$PORT"
export KAS_API_URL="$API"
export KAS_CODEX_BIN="$CODEX_BIN"

"$CORE_TARGET/debug/kas-migrate"
ADMIN_TOKEN="$("$CORE_TARGET/debug/kas-admin" bootstrap platform-admin)"
"$CORE_TARGET/debug/kas-api" >"$API_LOG" 2>&1 &
API_PID="$!"
trap - ERR
for _ in $(seq 1 200); do
  if command curl --fail --silent "$API/health" >/dev/null 2>&1; then
    break
  fi
  sleep 0.05
done
trap 'failed "$LINENO"' ERR
command curl --fail --silent "$API/health" | jq -e '.ok == true' >/dev/null

install_package() {
  request --fail-with-body --silent --show-error \
    -H "Authorization: Bearer $ADMIN_TOKEN" \
    -H "Content-Type: application/vnd.kas.manifest+tar" \
    --data-binary "@$1" \
    "$API/packages"
}

AGENT_PACKAGE="$(install_package "$E2E_DIR/packages/agent.kas")"
THREAD_PACKAGE="$(install_package "$E2E_DIR/packages/thread.kas")"
MESSAGE_PACKAGE="$(install_package "$E2E_DIR/packages/message.kas")"
for package in "$AGENT_PACKAGE" "$THREAD_PACKAGE" "$MESSAGE_PACKAGE"; do
  jq -e '.metadata.manifest == "/builtin/package"' <<<"$package" >/dev/null
done

for path in \
  /manifests/agent \
  /manifests/agent/actions/message \
  /manifests/thread \
  /manifests/thread/relations/participants \
  /manifests/message \
  /manifests/message/relations/authored-by \
  /manifests/message/relations/message-thread \
  /manifests/message/relations/mentioned \
  /manifests/message/relations/replies-to; do
  get_resource "$path" >/dev/null
done
if get_resource "/manifests/message/relations/thread-root" >/dev/null 2>&1; then
  echo "obsolete thread-root Relation still exists" >&2
  false
fi

AGENT_DRIVER="$(wait_for_state "/manifests/agent/driver" running)"
MESSAGE_DRIVER="$(wait_for_state "/manifests/message/driver" running)"
jq -e '.spec == .status.spec' <<<"$AGENT_DRIVER" >/dev/null
jq -e '.spec == .status.spec' <<<"$MESSAGE_DRIVER" >/dev/null

mkdir -p "$E2E_DIR/workspace"
AGENT_PATH="/agents/e2e"
OBSERVER_PATH="/agents/observer"
PROOF_PATH="/messages/e2e-agent-network-proof"
KAS_PROOF="KAS_NETWORK_$(uuidgen | tr '[:lower:]' '[:upper:]')"

create_agent() {
  local path="$1" name="$2" instructions="$3"
  post_resource "$(
    jq -n \
      --arg path "$path" \
      --arg name "$name" \
      --arg cwd "$E2E_DIR/workspace" \
      --arg instructions "$instructions" '{
        metadata: {
          path: $path,
          manifest: "/manifests/agent",
          name: $name
        },
        spec: {
          instructions: $instructions,
          working_directory: $cwd
        }
      }'
  )" >/dev/null
}

create_agent "$AGENT_PATH" "e2e-agent" \
  "Use curl with \$KAS_API and \$KAS_TOKEN to POST a Message Resource at $PROOF_PATH with name e2e-agent-network-proof and spec {\"role\":\"system\",\"body\":\"$KAS_PROOF\"}. After the POST succeeds, reply with exactly CREATED and no other text."
create_agent "$OBSERVER_PATH" "observer" "Reply with exactly OBSERVER."
wait_for_state "$AGENT_PATH" available >/dev/null
wait_for_state "$OBSERVER_PATH" available >/dev/null

for path in \
  "$AGENT_PATH/service-account" \
  "$AGENT_PATH/role-binding" \
  "$AGENT_PATH/links/service-account" \
  "$OBSERVER_PATH/service-account" \
  "$OBSERVER_PATH/role-binding" \
  "$OBSERVER_PATH/links/service-account"; do
  get_resource "$path" >/dev/null
done

THREAD_PATH="/threads/e2e"
post_resource "$(
  jq -n --arg path "$THREAD_PATH" '{
    metadata: {
      path: $path,
      manifest: "/manifests/thread",
      name: "e2e-thread"
    },
    spec: {title: "E2E multi-Agent Thread"}
  }'
)" >/dev/null
create_link "$THREAD_PATH/links/participants/user" \
  "/manifests/thread/relations/participants" "$THREAD_PATH" "/users/platform-admin"
create_link "$THREAD_PATH/links/participants/observer" \
  "/manifests/thread/relations/participants" "$THREAD_PATH" "$OBSERVER_PATH"

MESSAGE_PATH="/messages/e2e-user"
post_resource "$(
  jq -n --arg path "$MESSAGE_PATH" '{
    metadata: {
      path: $path,
      manifest: "/manifests/message",
      name: "e2e-user-message"
    },
    spec: {
      role: "user",
      body: "@e2e hello from platform e2e"
    }
  }'
)" >/dev/null
create_link "$MESSAGE_PATH/links/authored-by" \
  "/manifests/message/relations/authored-by" "$MESSAGE_PATH" "/users/platform-admin"
MENTION_LINK="$MESSAGE_PATH/links/mentioned/agents-e2e"
create_link "$MENTION_LINK" \
  "/manifests/message/relations/mentioned" "$MESSAGE_PATH" "$AGENT_PATH"

RUN_PATH="$MENTION_LINK/run"
create_link "$MESSAGE_PATH/links/message-thread" \
  "/manifests/message/relations/message-thread" "$MESSAGE_PATH" "$THREAD_PATH"
if get_resource "$RUN_PATH" >/dev/null 2>&1; then
  echo "Agent received a Run before it became a Thread participant" >&2
  false
fi
create_link "$THREAD_PATH/links/participants/e2e" \
  "/manifests/thread/relations/participants" "$THREAD_PATH" "$AGENT_PATH"

RUN="$(wait_for_run "$RUN_PATH")"
if [[ "$(jq -r '.status.metadata.state' <<<"$RUN")" != "succeeded" ]]; then
  echo "Agent Run failed: $(jq -c . <<<"$RUN")" >&2
  false
fi
jq -e --arg message "$MESSAGE_PATH" --arg thread "$THREAD_PATH" --arg agent "$AGENT_PATH" '
  .spec.resource == $agent
  and .spec.input == {message_path: $message, thread_path: $thread}
' <<<"$RUN" >/dev/null

OBSERVER_RUN="$MESSAGE_PATH/links/mentioned/agents-observer/run"
if get_resource "$OBSERVER_RUN" >/dev/null 2>&1; then
  echo "unmentioned observer Agent received a Run" >&2
  false
fi

PROOF="$(get_resource "$PROOF_PATH")"
jq -e --arg proof "$KAS_PROOF" '
  .metadata.manifest == "/manifests/message"
  and .spec == {role: "system", body: $proof}
' <<<"$PROOF" >/dev/null

REPLY_PATH="$(jq -r '.spec.output.reply_message_path' <<<"$RUN")"
REPLY="$(get_resource "$REPLY_PATH")"
jq -e '.spec == {role: "assistant", body: "CREATED"}' <<<"$REPLY" >/dev/null
LINKS="$(
  request --fail --silent --get \
    -H "Authorization: Bearer $ADMIN_TOKEN" \
    --data-urlencode "manifest=/builtin/link" \
    "$API/resources"
)"
jq -e --arg reply "$REPLY_PATH" --arg agent "$AGENT_PATH" --arg parent "$MESSAGE_PATH" --arg thread "$THREAD_PATH" '
  any(.[]; .spec.relation == "/manifests/message/relations/authored-by"
    and .spec.source == $reply and .spec.target == $agent)
  and any(.[]; .spec.relation == "/manifests/message/relations/replies-to"
    and .spec.source == $reply and .spec.target == $parent)
  and any(.[]; .spec.relation == "/manifests/message/relations/message-thread"
    and .spec.source == $reply and .spec.target == $thread)
  and ([.[] | select(.spec.relation == "/manifests/message/relations/thread-root")] | length) == 0
' <<<"$LINKS" >/dev/null

stop_driver() {
  local path="$1"
  request --fail --silent \
    -H "Authorization: Bearer $ADMIN_TOKEN" \
    -H "Content-Type: application/json" \
    -d "$(jq -cn --arg path "$path" '{path: $path, state: "stopped"}')" \
    "$API/drivers/control" >/dev/null
  wait_for_state "$path" stopped >/dev/null
}
stop_driver "/manifests/message/driver"
stop_driver "/manifests/agent/driver"

echo "KAS platform Thread/@mention end-to-end test passed"
