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

control_driver() {
  local path="$1" state="$2"
  request --fail --silent \
    -H "Authorization: Bearer $ADMIN_TOKEN" \
    -H "Content-Type: application/json" \
    -d "$(jq -cn --arg path "$path" --arg state "$state" '{path: $path, state: $state}')" \
    "$API/drivers/control" >/dev/null
  wait_for_state "$path" "$state" >/dev/null
}

cd "$ROOT"
CARGO_TARGET_DIR="$CORE_TARGET" cargo build --workspace
CARGO_TARGET_DIR="$PLATFORM_TARGET" "$PLATFORM_ROOT/scripts/build-packages.sh" "$E2E_DIR/packages"

PORT="$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1]); s.close()')"
FILE_PORT="$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1]); s.close()')"
API="http://127.0.0.1:$PORT"
FILE_API="http://127.0.0.1:$FILE_PORT"
export KAS_DATA_DIR="$E2E_DIR/data"
export KAS_DATABASE="$KAS_DATA_DIR/kas.db"
export KAS_ADDRESS="127.0.0.1:$PORT"
export KAS_API_URL="$API"
export KAS_FILE_ADDRESS="127.0.0.1:$FILE_PORT"
export KAS_FILE_API="$FILE_API"
export KAS_CODEX_BIN="$CODEX_BIN"
SOURCE_CODEX_HOME="${CODEX_HOME:-$HOME/.codex}"
export KAS_CODEX_HOME="$E2E_DIR/codex-home"
mkdir -p "$KAS_CODEX_HOME"
chmod 700 "$KAS_CODEX_HOME"
for entry in auth.json config.toml; do
  if [[ -e "$SOURCE_CODEX_HOME/$entry" ]]; then
    ln -s "$SOURCE_CODEX_HOME/$entry" "$KAS_CODEX_HOME/$entry"
  fi
done

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

THREAD_PACKAGE="$(install_package "$E2E_DIR/packages/thread.kas")"
SESSION_PACKAGE="$(install_package "$E2E_DIR/packages/session.kas")"
FILE_PACKAGE="$(install_package "$E2E_DIR/packages/file.kas")"
AGENT_PACKAGE="$(install_package "$E2E_DIR/packages/agent.kas")"
MESSAGE_PACKAGE="$(install_package "$E2E_DIR/packages/message.kas")"
for package in "$THREAD_PACKAGE" "$SESSION_PACKAGE" "$FILE_PACKAGE" "$AGENT_PACKAGE" "$MESSAGE_PACKAGE"; do
  jq -e '.metadata.manifest == "/builtin/package"' <<<"$package" >/dev/null
done

for path in \
  /manifests/agent \
  /manifests/agent/actions/message \
  /manifests/thread \
  /manifests/thread/relations/participants \
  /manifests/session \
  /manifests/session/relations/thread-session \
  /manifests/session/relations/agent-session \
  /manifests/file \
  /manifests/file/relations/attached-to \
  /manifests/file/relations/uploaded-by \
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
FILE_DRIVER="$(wait_for_state "/manifests/file/driver" running)"
MESSAGE_DRIVER="$(wait_for_state "/manifests/message/driver" running)"
jq -e '.spec == .status.spec' <<<"$AGENT_DRIVER" >/dev/null
jq -e '.spec == .status.spec' <<<"$FILE_DRIVER" >/dev/null
jq -e '.spec == .status.spec' <<<"$MESSAGE_DRIVER" >/dev/null
for _ in $(seq 1 200); do
  if command curl --fail --silent "$FILE_API/health" >/dev/null 2>&1; then
    break
  fi
  sleep 0.05
done
command curl --fail --silent "$FILE_API/health" | jq -e '.ok == true' >/dev/null

mkdir -p "$E2E_DIR/workspace"
AGENT_PATH="/agents/e2e"
OBSERVER_PATH="/agents/observer"
PROOF_PATH="/messages/e2e-agent-network-proof"
FILE_PROOF="KAS_FILE_$(uuidgen | tr '[:lower:]' '[:upper:]')"
SESSION_SECRET="KAS_SESSION_$(uuidgen | tr '[:lower:]' '[:upper:]')"
printf '%s' "$FILE_PROOF" >"$E2E_DIR/attachment.bin"
FILE="$(
  request --fail-with-body --silent --show-error \
    -H "Authorization: Bearer $ADMIN_TOKEN" \
    -F "content=@$E2E_DIR/attachment.bin;type=application/octet-stream" \
    "$FILE_API/files?path=/files/e2e-input"
)"
FILE_PATH="$(jq -r '.metadata.path' <<<"$FILE")"
jq -e --arg path "$FILE_PATH" '
  .metadata.manifest == "/manifests/file"
  and .metadata.path == $path
  and .spec.filename == "attachment.bin"
  and .spec.media_type == "application/octet-stream"
  and .spec.size > 0
  and (.spec.digest | test("^sha256:[0-9a-f]{64}$"))
  and (.spec.handle | length) > 0
' <<<"$FILE" >/dev/null
wait_for_state "$FILE_PATH" available >/dev/null
command curl --fail --silent --get \
  -H "Authorization: Bearer $ADMIN_TOKEN" \
  --data-urlencode "path=$FILE_PATH" \
  "$FILE_API/files/content" >"$E2E_DIR/downloaded.bin"
cmp "$E2E_DIR/attachment.bin" "$E2E_DIR/downloaded.bin"
command curl --fail --silent --get \
  -H "Authorization: Bearer $ADMIN_TOKEN" \
  -H "Range: bytes=4-7" \
  --data-urlencode "path=$FILE_PATH" \
  "$FILE_API/files/content" >"$E2E_DIR/range.bin"
cmp <(printf '%s' "${FILE_PROOF:4:4}") "$E2E_DIR/range.bin"
FILE_ONLY_MESSAGE="/messages/e2e-file-only"
post_resource "$(
  jq -n --arg path "$FILE_ONLY_MESSAGE" '{
    metadata: {
      path: $path,
      manifest: "/manifests/message",
      name: "e2e-file-only"
    },
    spec: {
      role: "user",
      body: ""
    }
  }'
)" >/dev/null
create_link "$FILE_ONLY_MESSAGE/links/attachments/e2e-input" \
  "/manifests/file/relations/attached-to" "$FILE_PATH" "$FILE_ONLY_MESSAGE"
wait_for_state "$FILE_ONLY_MESSAGE" available >/dev/null

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
  "Follow the latest user Message exactly. Preserve facts the user asks you to remember in this Codex Session."
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
OBSERVER_TOKEN="$(
  request --fail --silent \
    -H "Authorization: Bearer $ADMIN_TOKEN" \
    -H "Content-Type: application/json" \
    -d "{\"subject\":\"$OBSERVER_PATH/service-account\"}" \
    "$API/credentials/issue" |
    jq -r '.token'
)"
command curl --fail --silent --get \
  -H "Authorization: Bearer $OBSERVER_TOKEN" \
  --data-urlencode "path=$FILE_PATH" \
  "$FILE_API/files/content" >"$E2E_DIR/observer-download.bin"
cmp "$E2E_DIR/attachment.bin" "$E2E_DIR/observer-download.bin"
AGENT_UPLOAD_PATH="/files/agent-upload"
AGENT_UPLOAD="$(
  command curl --fail-with-body --silent --show-error \
    -H "Authorization: Bearer $OBSERVER_TOKEN" \
    -F "content=@$E2E_DIR/attachment.bin;type=application/octet-stream" \
    "$FILE_API/files?path=$AGENT_UPLOAD_PATH"
)"
jq -e --arg path "$AGENT_UPLOAD_PATH" '
  .metadata.path == $path and .metadata.manifest == "/manifests/file"
' <<<"$AGENT_UPLOAD" >/dev/null
OVERWRITE_STATUS="$(
  command curl --silent --output "$E2E_DIR/upload-conflict.json" --write-out "%{http_code}" \
    -H "Authorization: Bearer $OBSERVER_TOKEN" \
    -F "content=@$E2E_DIR/range.bin;type=application/octet-stream" \
    "$FILE_API/files?path=$AGENT_UPLOAD_PATH"
)"
[[ "$OVERWRITE_STATUS" == "409" ]]
command curl --fail --silent --get \
  -H "Authorization: Bearer $OBSERVER_TOKEN" \
  --data-urlencode "path=$AGENT_UPLOAD_PATH" \
  "$FILE_API/files/content" >"$E2E_DIR/agent-upload-download.bin"
cmp "$E2E_DIR/attachment.bin" "$E2E_DIR/agent-upload-download.bin"

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
  jq -n \
    --arg path "$MESSAGE_PATH" \
    --arg body "@e2e Remember $SESSION_SECRET. Download the attached File using the provided KAS_FILE_API command and read the downloaded bytes. Then use curl with \$KAS_API and \$KAS_TOKEN to POST a Message Resource at $PROOF_PATH with name e2e-agent-network-proof and spec.role system. Set spec.body to the actual exact text you read from the downloaded file; do not use a placeholder or angle brackets. After the POST succeeds, read the created Resource back and verify spec.body still equals the downloaded text, then reply with exactly CREATED and no other text." '{
    metadata: {
      path: $path,
      manifest: "/manifests/message",
      name: "e2e-user-message"
    },
    spec: {
      role: "user",
      body: $body
    }
  }'
)" >/dev/null
create_link "$MESSAGE_PATH/links/authored-by" \
  "/manifests/message/relations/authored-by" "$MESSAGE_PATH" "/users/platform-admin"
create_link "$MESSAGE_PATH/links/attachments/e2e-input" \
  "/manifests/file/relations/attached-to" "$FILE_PATH" "$MESSAGE_PATH"
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
jq -e --arg proof "$FILE_PROOF" '
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

SESSION_PATH="$THREAD_PATH/sessions/agents-e2e"
SESSION="$(wait_for_state "$SESSION_PATH" available)"
SESSION_ID="$(jq -r '.spec.session_id' <<<"$SESSION")"
jq -e --arg cursor "$REPLY_PATH" '
  .spec.provider == "codex"
  and (.spec.session_id | length) > 0
  and .spec.cursor == $cursor
  and .spec == .status.spec
' <<<"$SESSION" >/dev/null
for path in "$SESSION_PATH/links/thread" "$SESSION_PATH/links/agent"; do
  wait_for_state "$path" available >/dev/null
done
jq -e --arg session "$SESSION_PATH" --arg thread "$THREAD_PATH" --arg agent "$AGENT_PATH" '
  any(.[]; .spec.relation == "/manifests/session/relations/thread-session"
    and .spec.source == $thread and .spec.target == $session)
  and any(.[]; .spec.relation == "/manifests/session/relations/agent-session"
    and .spec.source == $agent and .spec.target == $session)
' <<<"$(
  request --fail --silent --get \
    -H "Authorization: Bearer $ADMIN_TOKEN" \
    --data-urlencode "manifest=/builtin/link" \
    "$API/resources"
)" >/dev/null

OBSERVER_SESSION="$THREAD_PATH/sessions/agents-observer"
if get_resource "$OBSERVER_SESSION" >/dev/null 2>&1; then
  echo "unmentioned observer Agent received a Session" >&2
  false
fi

# A Driver restart must not lose the provider Session mapping.
control_driver "/manifests/agent/driver" stopped
control_driver "/manifests/agent/driver" running

SECOND_MESSAGE_PATH="/messages/e2e-user-resume"
post_resource "$(
  jq -n \
    --arg path "$SECOND_MESSAGE_PATH" '{
      metadata: {
        path: $path,
        manifest: "/manifests/message",
        name: "e2e-user-resume"
      },
      spec: {
        role: "user",
        body: ("@e2e Reply with exactly the secret I asked you to remember in the previous turn. The expected format starts with KAS_NETWORK_, but do not invent a new value.")
      }
    }'
)" >/dev/null
create_link "$SECOND_MESSAGE_PATH/links/authored-by" \
  "/manifests/message/relations/authored-by" "$SECOND_MESSAGE_PATH" "/users/platform-admin"
create_link "$SECOND_MESSAGE_PATH/links/replies-to" \
  "/manifests/message/relations/replies-to" "$SECOND_MESSAGE_PATH" "$REPLY_PATH"
create_link "$SECOND_MESSAGE_PATH/links/message-thread" \
  "/manifests/message/relations/message-thread" "$SECOND_MESSAGE_PATH" "$THREAD_PATH"
SECOND_MENTION_LINK="$SECOND_MESSAGE_PATH/links/mentioned/agents-e2e"
create_link "$SECOND_MENTION_LINK" \
  "/manifests/message/relations/mentioned" "$SECOND_MESSAGE_PATH" "$AGENT_PATH"

SECOND_RUN="$(wait_for_run "$SECOND_MENTION_LINK/run")"
if [[ "$(jq -r '.status.metadata.state' <<<"$SECOND_RUN")" != "succeeded" ]]; then
  echo "resumed Agent Run failed: $(jq -c . <<<"$SECOND_RUN")" >&2
  false
fi
SECOND_REPLY_PATH="$(jq -r '.spec.output.reply_message_path' <<<"$SECOND_RUN")"
SECOND_REPLY="$(get_resource "$SECOND_REPLY_PATH")"
jq -e --arg secret "$SESSION_SECRET" '
  .spec == {role: "assistant", body: $secret}
' <<<"$SECOND_REPLY" >/dev/null

SESSION="$(wait_for_state "$SESSION_PATH" available)"
jq -e --arg id "$SESSION_ID" --arg cursor "$SECOND_REPLY_PATH" '
  .spec.provider == "codex"
  and .spec.session_id == $id
  and .spec.cursor == $cursor
  and .spec == .status.spec
' <<<"$SESSION" >/dev/null

FILE_RESOURCE="$(get_resource "$FILE_PATH")"
request --fail --silent --request DELETE --get \
  -H "Authorization: Bearer $ADMIN_TOKEN" \
  --data-urlencode "path=$FILE_PATH" \
  --data-urlencode "expected_revision=$(jq -r '.metadata["[kas]"].revision' <<<"$FILE_RESOURCE")" \
  "$API/resources/by-path" \
  >/dev/null
for _ in $(seq 1 400); do
  if ! get_resource "$FILE_PATH" >/dev/null 2>&1; then
    break
  fi
  sleep 0.05
done
if get_resource "$FILE_PATH" >/dev/null 2>&1; then
  echo "File Resource was not deleted after content reconciliation" >&2
  false
fi
DELETED_DOWNLOAD_STATUS="$(
  command curl --silent --output "$E2E_DIR/deleted-download.json" --write-out "%{http_code}" \
    --get \
    -H "Authorization: Bearer $ADMIN_TOKEN" \
    --data-urlencode "path=$FILE_PATH" \
    "$FILE_API/files/content"
)"
[[ "$DELETED_DOWNLOAD_STATUS" == "404" ]]

control_driver "/manifests/message/driver" stopped
control_driver "/manifests/agent/driver" stopped
control_driver "/manifests/file/driver" stopped

echo "KAS platform File and persistent Session end-to-end test passed"
