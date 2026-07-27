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

for command in cargo curl jq sqlite3 tar uuidgen python3 zip; do
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
SKILL_PORT="$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1]); s.close()')"
APPROVAL_PORT="$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1]); s.close()')"
FRONTEND_PORT="$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1]); s.close()')"
API="http://127.0.0.1:$PORT"
FILE_API="http://127.0.0.1:$FILE_PORT"
SKILL_API="http://127.0.0.1:$SKILL_PORT"
APPROVAL_API="http://127.0.0.1:$APPROVAL_PORT"
FRONTEND="http://127.0.0.1:$FRONTEND_PORT"
export KAS_DATA_DIR="$E2E_DIR/data"
export KAS_DATABASE="$KAS_DATA_DIR/kas.db"
export KAS_ADDRESS="127.0.0.1:$PORT"
export KAS_API_URL="$API"
export KAS_FILE_ADDRESS="127.0.0.1:$FILE_PORT"
export KAS_FILE_API="$FILE_API"
export KAS_SKILL_ADDRESS="127.0.0.1:$SKILL_PORT"
export KAS_SKILL_API="$SKILL_API"
export KAS_APPROVAL_ADDRESS="127.0.0.1:$APPROVAL_PORT"
export KAS_APPROVAL_API="$APPROVAL_API"
export KAS_APPROVAL_API_URL="$APPROVAL_API"
export KAS_FRONTEND_ADDRESS="127.0.0.1:$FRONTEND_PORT"
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
PROXY_PACKAGE="$(install_package "$E2E_DIR/packages/proxy.kas")"
FRONTEND_PACKAGE="$(install_package "$E2E_DIR/packages/frontend.kas")"
SKILL_PACKAGE="$(install_package "$E2E_DIR/packages/skill.kas")"
AGENT_PACKAGE="$(install_package "$E2E_DIR/packages/agent.kas")"
MESSAGE_PACKAGE="$(install_package "$E2E_DIR/packages/message.kas")"
APPROVAL_RESULT_PACKAGE="$(install_package "$E2E_DIR/packages/approval-result.kas")"
APPROVAL_PACKAGE="$(install_package "$E2E_DIR/packages/approval.kas")"
for package in "$THREAD_PACKAGE" "$SESSION_PACKAGE" "$FILE_PACKAGE" "$PROXY_PACKAGE" "$FRONTEND_PACKAGE" "$SKILL_PACKAGE" "$AGENT_PACKAGE" "$MESSAGE_PACKAGE" "$APPROVAL_RESULT_PACKAGE" "$APPROVAL_PACKAGE"; do
  jq -e '.metadata.manifest == "/builtin/package"' <<<"$package" >/dev/null
done

create_proxy() {
  local path="$1" name="$2" prefix="$3" upstream="$4"
  post_resource "$(
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
  )" >/dev/null
}
create_proxy "/proxies/file" "File API" "/files-api" "$FILE_API"
create_proxy "/proxies/skill" "Skill API" "/skills-api" "$SKILL_API"
create_proxy "/proxies/approval" "Approval API" "/approvals-api" "$APPROVAL_API"

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
  /manifests/proxy \
  /proxies/file \
  /proxies/skill \
  /proxies/approval \
  /manifests/frontend-plugin \
  /manifests/frontend-plugin/relations/bundle \
  /manifests/skill \
  /manifests/skill/relations/bundle \
  /manifests/skill/relations/owns \
  /manifests/skill/relations/uses \
  /skills/kas \
  /manifests/message \
  /manifests/message/relations/authored-by \
  /manifests/message/relations/message-thread \
  /manifests/message/relations/mentioned \
  /manifests/message/relations/replies-to \
  /manifests/approval \
  /manifests/approval-result \
  /manifests/approval/relations/requested-by \
  /manifests/approval/relations/decides \
  /manifests/approval/relations/decided-by \
  /manifests/approval/relations/result-of \
  /manifests/approval/relations/produced-by; do
  get_resource "$path" >/dev/null
done
if get_resource "/manifests/message/relations/thread-root" >/dev/null 2>&1; then
  echo "obsolete thread-root Relation still exists" >&2
  false
fi

AGENT_DRIVER="$(wait_for_state "/manifests/agent/driver" running)"
FILE_DRIVER="$(wait_for_state "/manifests/file/driver" running)"
FRONTEND_DRIVER="$(wait_for_state "/manifests/frontend-plugin/driver" running)"
SKILL_DRIVER="$(wait_for_state "/manifests/skill/driver" running)"
MESSAGE_DRIVER="$(wait_for_state "/manifests/message/driver" running)"
APPROVAL_DRIVER="$(wait_for_state "/manifests/approval/driver" running)"
FILE_PROXY="$(wait_for_state "/proxies/file" available)"
SKILL_PROXY="$(wait_for_state "/proxies/skill" available)"
APPROVAL_PROXY="$(wait_for_state "/proxies/approval" available)"
jq -e '.spec == .status.spec' <<<"$AGENT_DRIVER" >/dev/null
jq -e '.spec == .status.spec' <<<"$FILE_DRIVER" >/dev/null
jq -e '.spec == .status.spec' <<<"$FRONTEND_DRIVER" >/dev/null
jq -e '.spec == .status.spec' <<<"$SKILL_DRIVER" >/dev/null
jq -e '.spec == .status.spec' <<<"$MESSAGE_DRIVER" >/dev/null
jq -e '.spec == .status.spec' <<<"$APPROVAL_DRIVER" >/dev/null
jq -e '.spec == .status.spec' <<<"$FILE_PROXY" >/dev/null
jq -e '.spec == .status.spec' <<<"$SKILL_PROXY" >/dev/null
jq -e '.spec == .status.spec' <<<"$APPROVAL_PROXY" >/dev/null
for _ in $(seq 1 200); do
  if command curl --fail --silent "$FILE_API/health" >/dev/null 2>&1; then
    break
  fi
  sleep 0.05
done
command curl --fail --silent "$FILE_API/health" | jq -e '.ok == true' >/dev/null
"$PLATFORM_ROOT/scripts/build-frontend-plugin.sh" \
  "$PLATFORM_ROOT/plugins/registry" \
  "$E2E_DIR/registry.zip"
KAS_API_URL="$API" \
KAS_FILE_API_URL="$FILE_API" \
KAS_TOKEN="$ADMIN_TOKEN" \
  "$PLATFORM_ROOT/scripts/install-frontend-plugin.sh" \
    "$E2E_DIR/registry.zip" \
    "/frontend-plugins/e2e-registry" \
    "e2e-registry" \
    "index.html" \
    "E2E Registry" \
    "◇" \
    "50" \
    "/e2e-registry" >/dev/null
FRONTEND_PLUGIN="$(wait_for_state "/frontend-plugins/e2e-registry" available)"
jq -e '
  .metadata.manifest == "/manifests/frontend-plugin"
  and .metadata.state == "available"
  and .status.metadata.state == "available"
  and .spec.api_version == 1
' <<<"$FRONTEND_PLUGIN" >/dev/null
FRONTEND_PLUGIN_LINK="$(get_resource "/frontend-plugins/e2e-registry/links/bundle")"
FRONTEND_PLUGIN_FILE="$(jq -r '.spec.target' <<<"$FRONTEND_PLUGIN_LINK")"
get_resource "$FRONTEND_PLUGIN_FILE" >/dev/null
for _ in $(seq 1 200); do
  if command curl --fail --silent "$FRONTEND/health" >/dev/null 2>&1; then
    break
  fi
  sleep 0.05
done
command curl --fail --silent "$FRONTEND/health" | jq -e '.ok == true' >/dev/null
COOKIE_JAR="$E2E_DIR/frontend.cookies"
command curl --fail --silent \
  -c "$COOKIE_JAR" \
  -H "Content-Type: application/json" \
  -d "$(jq -cn --arg token "$ADMIN_TOKEN" '{token:$token}')" \
  "$FRONTEND/gateway/session" | jq -e '.subject != null' >/dev/null
command curl --fail --silent -b "$COOKIE_JAR" "$FRONTEND/api/health" | jq -e '.ok == true' >/dev/null
command curl --fail --silent -b "$COOKIE_JAR" "$FRONTEND/files-api/health" | jq -e '.ok == true' >/dev/null
command curl --fail --silent -b "$COOKIE_JAR" "$FRONTEND/skills-api/health" | jq -e '.ok == true' >/dev/null
command curl --fail --silent -b "$COOKIE_JAR" "$FRONTEND/approvals-api/health" | jq -e '.ok == true' >/dev/null
command curl --fail --silent -b "$COOKIE_JAR" "$FRONTEND/plugins/e2e-registry/index.html" |
  grep -q "Object Registry"
for _ in $(seq 1 200); do
  if command curl --fail --silent "$SKILL_API/health" >/dev/null 2>&1; then
    break
  fi
  sleep 0.05
done
command curl --fail --silent "$SKILL_API/health" | jq -e '.ok == true' >/dev/null
for _ in $(seq 1 200); do
  if command curl --fail --silent "$APPROVAL_API/health" >/dev/null 2>&1; then
    break
  fi
  sleep 0.05
done
command curl --fail --silent "$APPROVAL_API/health" | jq -e '.ok == true' >/dev/null
KAS_SKILL="$(wait_for_state "/skills/kas" available)"
jq -e '
  .metadata.state == "available"
  and .status.metadata.state == "available"
  and .spec == .status.spec
' <<<"$KAS_SKILL" >/dev/null

mkdir -p "$E2E_DIR/workspace"
AGENT_PATH="/agents/e2e"
OBSERVER_PATH="/agents/observer"
PROOF_PATH="/messages/e2e-agent-network-proof"
SKILL_PROOF_PATH="/messages/e2e-agent-skill-proof"
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

SKILL_V1_BUNDLE="$E2E_DIR/e2e-v1.skill"
SKILL_V2_BUNDLE="$E2E_DIR/e2e-v2.skill"
(cd "$PLATFORM_ROOT/tests/fixtures/skills/e2e-v1" && zip -qr "$SKILL_V1_BUNDLE" .)
(cd "$PLATFORM_ROOT/tests/fixtures/skills/e2e-v2" && zip -qr "$SKILL_V2_BUNDLE" .)

INVALID_SKILL_DIR="$E2E_DIR/invalid-skill"
cp -R "$PLATFORM_ROOT/tests/fixtures/skills/e2e-v1" "$INVALID_SKILL_DIR"
ln -s ../../outside "$INVALID_SKILL_DIR/scripts-link"
INVALID_SKILL_BUNDLE="$E2E_DIR/invalid.skill"
(cd "$INVALID_SKILL_DIR" && zip -qry "$INVALID_SKILL_BUNDLE" .)
INVALID_SKILL_STATUS="$(
  command curl --silent --output "$E2E_DIR/invalid-skill.json" --write-out "%{http_code}" \
    -H "Authorization: Bearer $ADMIN_TOKEN" \
    -F "bundle=@$INVALID_SKILL_BUNDLE;type=application/zip" \
    "$SKILL_API/skills?path=/skills/invalid"
)"
[[ "$INVALID_SKILL_STATUS" == "400" ]]
if get_resource "/skills/invalid" >/dev/null 2>&1; then
  echo "invalid symlink Skill Bundle created a Skill Resource" >&2
  false
fi

SKILL_PATH="/skills/e2e-bundle"
SKILL="$(
  request --fail-with-body --silent --show-error \
    -H "Authorization: Bearer $ADMIN_TOKEN" \
    -F "bundle=@$SKILL_V1_BUNDLE;type=application/zip" \
    "$SKILL_API/skills?path=$SKILL_PATH"
)"
jq -e --arg path "$SKILL_PATH" '
  .metadata.path == $path
  and .metadata.manifest == "/manifests/skill"
  and .spec.name == "e2e-bundle"
' <<<"$SKILL" >/dev/null
wait_for_state "$SKILL_PATH" available >/dev/null
INITIAL_SKILL_LINK="$(get_resource "$SKILL_PATH/links/bundle")"
INITIAL_SKILL_FILE="$(jq -r '.spec.target' <<<"$INITIAL_SKILL_LINK")"
get_resource "$INITIAL_SKILL_FILE" >/dev/null
get_resource "$SKILL_PATH/links/owner" >/dev/null

create_agent() {
  local path="$1" name="$2"
  post_resource "$(
    jq -n \
      --arg path "$path" \
      --arg name "$name" \
      --arg cwd "$E2E_DIR/workspace" '{
        metadata: {
          path: $path,
          manifest: "/manifests/agent",
          name: $name
        },
        spec: {
          working_directory: $cwd
        }
      }'
  )" >/dev/null
}

create_agent "$AGENT_PATH" "e2e-agent"
create_agent "$OBSERVER_PATH" "observer"
wait_for_state "$AGENT_PATH" available >/dev/null
wait_for_state "$OBSERVER_PATH" available >/dev/null

for path in \
  "$AGENT_PATH/service-account" \
  "$AGENT_PATH/role-binding" \
  "$AGENT_PATH/skill-role" \
  "$AGENT_PATH/skill-role-binding" \
  "$AGENT_PATH/links/service-account" \
  "$AGENT_PATH/links/skills/kas" \
  "$OBSERVER_PATH/service-account" \
  "$OBSERVER_PATH/role-binding" \
  "$OBSERVER_PATH/skill-role" \
  "$OBSERVER_PATH/skill-role-binding" \
  "$OBSERVER_PATH/links/skills/kas" \
  "$OBSERVER_PATH/links/service-account"; do
  get_resource "$path" >/dev/null
done
post_resource "$(
  jq -n \
    --arg path "$AGENT_PATH/links/skills/e2e-bundle" \
    --arg source "$AGENT_PATH" \
    --arg target "$SKILL_PATH" '{
      metadata: {
        path: $path,
        manifest: "/builtin/link",
        name: "e2e-bundle"
      },
      spec: {
        relation: "/manifests/skill/relations/uses",
        source: $source,
        target: $target,
        metadata: {mode: "available"}
      }
    }'
)" >/dev/null
OBSERVER_TOKEN="$(
  request --fail --silent \
    -H "Authorization: Bearer $ADMIN_TOKEN" \
    -H "Content-Type: application/json" \
    -d "{\"subject\":\"$OBSERVER_PATH/service-account\"}" \
    "$API/credentials/issue" |
    jq -r '.token'
)"
AGENT_TOKEN="$(
  request --fail --silent \
    -H "Authorization: Bearer $ADMIN_TOKEN" \
    -H "Content-Type: application/json" \
    -d "{\"subject\":\"$AGENT_PATH/service-account\"}" \
    "$API/credentials/issue" |
    jq -r '.token'
)"

# Approval objects use their owning principal's path as their authorization
# boundary. Their business relationships are represented only by Links.
approval_links() {
  request --fail-with-body --silent --show-error --get \
    -H "Authorization: Bearer $ADMIN_TOKEN" \
    --data-urlencode "manifest=/builtin/link" \
    "$API/resources"
}

approval_link_source() {
  local relation="$1" target="$2"
  approval_links | jq -er \
    --arg relation "$relation" \
    --arg target "$target" '
      first(
        .[]
        | select(
            .spec.relation == $relation
            and .spec.target == $target
          )
        | .spec.source
      )
    '
}

assert_approval_link() {
  local relation="$1" source="$2" target="$3"
  approval_links | jq -e \
    --arg relation "$relation" \
    --arg source "$source" \
    --arg target "$target" '
      any(.[];
        .spec.relation == $relation
        and .spec.source == $source
        and .spec.target == $target
      )
    ' >/dev/null
}

assert_no_per_approval_rbac() {
  local request_path="$1"
  for manifest in /builtin/role /builtin/role-binding; do
    request --fail-with-body --silent --show-error --get \
      -H "Authorization: Bearer $ADMIN_TOKEN" \
      --data-urlencode "manifest=$manifest" \
      "$API/resources" |
      jq -e --arg request "$request_path" '
        all(.[];
          (.metadata.path | startswith($request + "/") | not)
          and ((.spec | tostring | contains($request)) | not)
        )
      ' >/dev/null
  done
}

# An Agent cannot perform this privileged write directly, but may request that
# an approving User execute the exact operation.
APPROVAL_TARGET="/approval-proofs/e2e-role"
APPROVAL_RESOURCE="$(
  jq -n --arg path "$APPROVAL_TARGET" '{
    metadata: {
      path: $path,
      manifest: "/builtin/role",
      name: "e2e-approved-role"
    },
    spec: {
      description: "Created only through an approved elevation",
      rules: []
    }
  }'
)"
DIRECT_APPROVAL_WRITE_STATUS="$(
  command curl --silent \
    --output "$E2E_DIR/direct-approval-write.json" \
    --write-out "%{http_code}" \
    -H "Authorization: Bearer $OBSERVER_TOKEN" \
    -H "Content-Type: application/json" \
    -d "$APPROVAL_RESOURCE" \
    "$API/resources"
)"
[[ "$DIRECT_APPROVAL_WRITE_STATUS" == "403" ]]
if get_resource "$APPROVAL_TARGET" >/dev/null 2>&1; then
  echo "Agent direct privileged write unexpectedly created the target Role" >&2
  false
fi

APPROVAL_REQUEST="$(
  request --fail-with-body --silent --show-error \
    -H "Authorization: Bearer $OBSERVER_TOKEN" \
    -H "Content-Type: application/json" \
    -d "$(
      jq -n --argjson resource "$APPROVAL_RESOURCE" '{
        reason: "E2E approval elevation proof",
        operation: {
          verb: "create",
          resource: $resource
        },
        expires_in_seconds: 300
      }'
    )" \
    "$APPROVAL_API/approvals"
)"
APPROVAL_PATH="$(jq -r '.metadata.path' <<<"$APPROVAL_REQUEST")"
APPROVAL_REVISION="$(jq -r '.metadata["[kas]"].revision' <<<"$APPROVAL_REQUEST")"
jq -e \
  --arg target "$APPROVAL_TARGET" '
    .metadata.manifest == "/manifests/approval"
    and .metadata.state == "pending"
    and .spec.kind == "request"
    and (.spec | has("requested_by") | not)
    and (.spec | has("requester_subject") | not)
    and .spec.operation.verb == "create"
    and .spec.operation.resource.metadata.path == $target
  ' <<<"$APPROVAL_REQUEST" >/dev/null
[[ "$APPROVAL_PATH" == "/approvals$OBSERVER_PATH/requests/"* ]]
assert_approval_link \
  "/manifests/approval/relations/requested-by" \
  "$APPROVAL_PATH" \
  "$OBSERVER_PATH"
assert_no_per_approval_rbac "$APPROVAL_PATH"

APPROVAL_DECISION="$(
  request --fail-with-body --silent --show-error \
    -H "Authorization: Bearer $ADMIN_TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"decision":"approve"}' \
    "$APPROVAL_API/approvals/decide?path=$APPROVAL_PATH&expected_revision=$APPROVAL_REVISION"
)"
APPROVAL_DECISION_PATH="$(jq -r '.metadata.path' <<<"$APPROVAL_DECISION")"
jq -e \
  --arg decision_prefix "/approvals/users/platform-admin/decisions/" '
    (.metadata.path | startswith($decision_prefix))
    and .metadata.manifest == "/manifests/approval"
    and .spec.kind == "decision"
    and (.spec | has("approval") | not)
    and .spec.outcome == "succeeded"
    and (.spec | has("decided_by") | not)
    and (.spec | has("result_path") | not)
    and .spec.error == null
  ' <<<"$APPROVAL_DECISION" >/dev/null
assert_approval_link \
  "/manifests/approval/relations/decides" \
  "$APPROVAL_DECISION_PATH" \
  "$APPROVAL_PATH"
assert_approval_link \
  "/manifests/approval/relations/decided-by" \
  "$APPROVAL_DECISION_PATH" \
  "/users/platform-admin"

APPROVAL_RESULT_PATH="$(
  approval_link_source \
    "/manifests/approval/relations/result-of" \
    "$APPROVAL_PATH"
)"
[[ "$APPROVAL_RESULT_PATH" == "/approvals$OBSERVER_PATH/results/"* ]]
APPROVAL_RESULT="$(get_resource "$APPROVAL_RESULT_PATH")"
jq -e \
  --arg result "$APPROVAL_RESULT_PATH" \
  --arg target "$APPROVAL_TARGET" '
    .metadata.path == $result
    and .metadata.manifest == "/manifests/approval-result"
    and (.spec | has("approval") | not)
    and .spec.response.status == 201
    and (.spec.response.content_type | startswith("application/json"))
    and .spec.response.body.metadata.path == $target
    and .spec.response.body.metadata.manifest == "/builtin/role"
    and .spec.response.body.metadata["[kas]"] == null
    and .spec.response.body.status.metadata["[kas]"] == null
  ' <<<"$APPROVAL_RESULT" >/dev/null
assert_approval_link \
  "/manifests/approval/relations/result-of" \
  "$APPROVAL_RESULT_PATH" \
  "$APPROVAL_PATH"
assert_approval_link \
  "/manifests/approval/relations/produced-by" \
  "$APPROVAL_RESULT_PATH" \
  "$APPROVAL_DECISION_PATH"
assert_no_per_approval_rbac "$APPROVAL_PATH"

APPROVED_TARGET="$(get_resource "$APPROVAL_TARGET")"
jq -e '
  .metadata.manifest == "/builtin/role"
  and .spec.description == "Created only through an approved elevation"
  and .spec.rules == []
' <<<"$APPROVED_TARGET" >/dev/null
wait_for_state "$APPROVAL_PATH" succeeded >/dev/null

DUPLICATE_APPROVAL_STATUS="$(
  command curl --silent \
    --output "$E2E_DIR/duplicate-approval.json" \
    --write-out "%{http_code}" \
    -H "Authorization: Bearer $ADMIN_TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"decision":"approve"}' \
    "$APPROVAL_API/approvals/decide?path=$APPROVAL_PATH&expected_revision=$APPROVAL_REVISION"
)"
[[ "$DUPLICATE_APPROVAL_STATUS" == "409" ]]

# Rejection is terminal and creates no target object or Result.
REJECTED_TARGET="/approval-proofs/rejected-role"
REJECTED_REQUEST="$(
  request --fail-with-body --silent --show-error \
    -H "Authorization: Bearer $OBSERVER_TOKEN" \
    -H "Content-Type: application/json" \
    -d "$(
      jq -n --arg path "$REJECTED_TARGET" '{
        reason: "E2E rejection proof",
        operation: {
          verb: "create",
          resource: {
            metadata: {
              path: $path,
              manifest: "/builtin/role",
              name: "e2e-rejected-role"
            },
            spec: {
              description: "This Role must not be created",
              rules: []
            }
          }
        },
        expires_in_seconds: 300
      }'
    )" \
    "$APPROVAL_API/approvals"
)"
REJECTED_APPROVAL_PATH="$(jq -r '.metadata.path' <<<"$REJECTED_REQUEST")"
REJECTED_APPROVAL_REVISION="$(jq -r '.metadata["[kas]"].revision' <<<"$REJECTED_REQUEST")"
REJECTED_DECISION="$(
  request --fail-with-body --silent --show-error \
    -H "Authorization: Bearer $ADMIN_TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"decision":"reject"}' \
    "$APPROVAL_API/approvals/decide?path=$REJECTED_APPROVAL_PATH&expected_revision=$REJECTED_APPROVAL_REVISION"
)"
jq -e \
  --arg decision_prefix "/approvals/users/platform-admin/decisions/" '
    (.metadata.path | startswith($decision_prefix))
    and .spec.kind == "decision"
    and .spec.outcome == "rejected"
    and (.spec | has("approval") | not)
    and (.spec | has("decided_by") | not)
    and (.spec | has("credential_path") | not)
    and (.spec | has("result_path") | not)
    and .spec.error == null
  ' <<<"$REJECTED_DECISION" >/dev/null
REJECTED_DECISION_PATH="$(jq -r '.metadata.path' <<<"$REJECTED_DECISION")"
assert_approval_link \
  "/manifests/approval/relations/decides" \
  "$REJECTED_DECISION_PATH" \
  "$REJECTED_APPROVAL_PATH"
assert_approval_link \
  "/manifests/approval/relations/decided-by" \
  "$REJECTED_DECISION_PATH" \
  "/users/platform-admin"
wait_for_state "$REJECTED_APPROVAL_PATH" rejected >/dev/null
if get_resource "$REJECTED_TARGET" >/dev/null 2>&1; then
  echo "rejected Approval unexpectedly created its target Role" >&2
  false
fi
if approval_links | jq -e \
  --arg relation "/manifests/approval/relations/result-of" \
  --arg request "$REJECTED_APPROVAL_PATH" '
    any(.[];
      .spec.relation == $relation
      and .spec.target == $request
    )
  ' >/dev/null; then
  echo "rejected Approval unexpectedly created a Result Link" >&2
  false
fi
assert_no_per_approval_rbac "$REJECTED_APPROVAL_PATH"

# Read approvals store the sanitized API response in a separately protected
# Approval Result. The requesting Agent may read it, while an unrelated Agent
# cannot read the request or result. The Decision belongs to the approving User.
LIMITED_APPROVER_PATH="/users/e2e-limited-approver"
post_resource "$(
  jq -n --arg path "$LIMITED_APPROVER_PATH" '{
    metadata: {
      path: $path,
      manifest: "/builtin/user",
      name: "e2e-limited-approver"
    },
    spec: {
      disabled: false
    }
  }'
)" >/dev/null
LIMITED_APPROVER_TOKEN="$(
  request --fail --silent \
    -H "Authorization: Bearer $ADMIN_TOKEN" \
    -H "Content-Type: application/json" \
    -d "{\"subject\":\"$LIMITED_APPROVER_PATH\"}" \
    "$API/credentials/issue" |
    jq -r '.token'
)"
GET_APPROVAL_REQUEST="$(
  request --fail-with-body --silent --show-error \
    -H "Authorization: Bearer $OBSERVER_TOKEN" \
    -H "Content-Type: application/json" \
    -d "$(
      jq -n --arg path "$APPROVAL_TARGET" '{
        reason: "E2E privileged read proof",
        operation: {
          verb: "get",
          path: $path
        },
        expires_in_seconds: 300
      }'
    )" \
    "$APPROVAL_API/approvals"
)"
GET_APPROVAL_PATH="$(jq -r '.metadata.path' <<<"$GET_APPROVAL_REQUEST")"
GET_APPROVAL_REVISION="$(jq -r '.metadata["[kas]"].revision' <<<"$GET_APPROVAL_REQUEST")"
INVALID_GET_DECISION="$(
  request --fail-with-body --silent --show-error \
    -H "Authorization: Bearer $LIMITED_APPROVER_TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"decision":"approve"}' \
    "$APPROVAL_API/approvals/decide?path=$GET_APPROVAL_PATH&expected_revision=$GET_APPROVAL_REVISION"
)"
INVALID_GET_DECISION_PATH="$(jq -r '.metadata.path' <<<"$INVALID_GET_DECISION")"
jq -e \
  --arg prefix "/approvals/users/e2e-limited-approver/decisions/" '
    (.metadata.path | startswith($prefix))
    and .spec.kind == "decision"
    and .spec.outcome == "invalid"
    and (.spec.error | length) > 0
  ' <<<"$INVALID_GET_DECISION" >/dev/null
assert_approval_link \
  "/manifests/approval/relations/decides" \
  "$INVALID_GET_DECISION_PATH" \
  "$GET_APPROVAL_PATH"
assert_approval_link \
  "/manifests/approval/relations/decided-by" \
  "$INVALID_GET_DECISION_PATH" \
  "$LIMITED_APPROVER_PATH"
jq -e '
  .metadata.state == "pending"
  and .status.metadata.state == "pending"
' <<<"$(get_resource "$GET_APPROVAL_PATH")" >/dev/null
if approval_links | jq -e \
  --arg relation "/manifests/approval/relations/result-of" \
  --arg request "$GET_APPROVAL_PATH" '
    any(.[];
      .spec.relation == $relation
      and .spec.target == $request
    )
  ' >/dev/null; then
  echo "invalid Approval Decision unexpectedly created a Result Link" >&2
  false
fi

GET_APPROVAL_DECISION="$(
  request --fail-with-body --silent --show-error \
    -H "Authorization: Bearer $ADMIN_TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"decision":"approve"}' \
    "$APPROVAL_API/approvals/decide?path=$GET_APPROVAL_PATH&expected_revision=$GET_APPROVAL_REVISION"
)"
GET_APPROVAL_DECISION_PATH="$(jq -r '.metadata.path' <<<"$GET_APPROVAL_DECISION")"
[[ "$GET_APPROVAL_DECISION_PATH" == "/approvals/users/platform-admin/decisions/"* ]]
assert_approval_link \
  "/manifests/approval/relations/decides" \
  "$GET_APPROVAL_DECISION_PATH" \
  "$GET_APPROVAL_PATH"
GET_APPROVAL_RESULT_PATH="$(
  approval_link_source \
    "/manifests/approval/relations/result-of" \
    "$GET_APPROVAL_PATH"
)"
[[ "$GET_APPROVAL_RESULT_PATH" == "/approvals$OBSERVER_PATH/results/"* ]]
GET_APPROVAL_RESULT="$(
  request --fail-with-body --silent --show-error --get \
    -H "Authorization: Bearer $OBSERVER_TOKEN" \
    --data-urlencode "path=$GET_APPROVAL_RESULT_PATH" \
    "$API/resources/by-path"
)"
jq -e \
  --arg target "$APPROVAL_TARGET" '
    .metadata.manifest == "/manifests/approval-result"
    and .spec.response.status == 200
    and (.spec.response.content_type | startswith("application/json"))
    and .spec.response.body.metadata.path == $target
    and .spec.response.body.metadata.manifest == "/builtin/role"
    and .spec.response.body.metadata["[kas]"] == null
    and .spec.response.body.status.metadata["[kas]"] == null
  ' <<<"$GET_APPROVAL_RESULT" >/dev/null
assert_approval_link \
  "/manifests/approval/relations/produced-by" \
  "$GET_APPROVAL_RESULT_PATH" \
  "$GET_APPROVAL_DECISION_PATH"

for visible_path in \
  "$GET_APPROVAL_PATH" \
  "$GET_APPROVAL_RESULT_PATH"; do
  request --fail-with-body --silent --show-error --get \
    -H "Authorization: Bearer $OBSERVER_TOKEN" \
    --data-urlencode "path=$visible_path" \
    "$API/resources/by-path" >/dev/null
done

for private_path in \
  "$GET_APPROVAL_PATH" \
  "$GET_APPROVAL_RESULT_PATH"; do
  PRIVATE_STATUS="$(
    command curl --silent \
      --output "$E2E_DIR/private-approval.json" \
      --write-out "%{http_code}" \
      --get \
      -H "Authorization: Bearer $AGENT_TOKEN" \
      --data-urlencode "path=$private_path" \
      "$API/resources/by-path"
  )"
  [[ "$PRIVATE_STATUS" == "403" ]]
done

DECISION_REQUESTER_STATUS="$(
  command curl --silent \
    --output "$E2E_DIR/private-approval-decision.json" \
    --write-out "%{http_code}" \
    --get \
    -H "Authorization: Bearer $OBSERVER_TOKEN" \
    --data-urlencode "path=$GET_APPROVAL_DECISION_PATH" \
    "$API/resources/by-path"
)"
[[ "$DECISION_REQUESTER_STATUS" == "403" ]]
assert_no_per_approval_rbac "$GET_APPROVAL_PATH"

# LIST freezes its manifest, path prefix, and limit in the approved operation.
LIST_APPROVAL_REQUEST="$(
  request --fail-with-body --silent --show-error \
    -H "Authorization: Bearer $OBSERVER_TOKEN" \
    -H "Content-Type: application/json" \
    -d "$(
      jq -n '{
        reason: "E2E privileged list proof",
        operation: {
          verb: "list",
          manifest: "/builtin/role",
          path_prefix: "/approval-proofs/",
          limit: 1
        },
        expires_in_seconds: 300
      }'
    )" \
    "$APPROVAL_API/approvals"
)"
LIST_APPROVAL_PATH="$(jq -r '.metadata.path' <<<"$LIST_APPROVAL_REQUEST")"
LIST_APPROVAL_REVISION="$(jq -r '.metadata["[kas]"].revision' <<<"$LIST_APPROVAL_REQUEST")"
LIST_APPROVAL_DECISION="$(
  request --fail-with-body --silent --show-error \
    -H "Authorization: Bearer $ADMIN_TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"decision":"approve"}' \
    "$APPROVAL_API/approvals/decide?path=$LIST_APPROVAL_PATH&expected_revision=$LIST_APPROVAL_REVISION"
)"
LIST_APPROVAL_DECISION_PATH="$(jq -r '.metadata.path' <<<"$LIST_APPROVAL_DECISION")"
LIST_APPROVAL_RESULT_PATH="$(
  approval_link_source \
    "/manifests/approval/relations/result-of" \
    "$LIST_APPROVAL_PATH"
)"
assert_approval_link \
  "/manifests/approval/relations/produced-by" \
  "$LIST_APPROVAL_RESULT_PATH" \
  "$LIST_APPROVAL_DECISION_PATH"
LIST_APPROVAL_RESULT="$(
  request --fail-with-body --silent --show-error --get \
    -H "Authorization: Bearer $OBSERVER_TOKEN" \
    --data-urlencode "path=$LIST_APPROVAL_RESULT_PATH" \
    "$API/resources/by-path"
)"
jq -e '
  .metadata.manifest == "/manifests/approval-result"
  and .spec.response.status == 200
  and (.spec.response.content_type | startswith("application/json"))
  and (.spec.response.body | length) == 1
  and all(.spec.response.body[];
    (.metadata.path | startswith("/approval-proofs/"))
    and .metadata.manifest == "/builtin/role"
    and .metadata["[kas]"] == null
    and .status.metadata["[kas]"] == null)
' <<<"$LIST_APPROVAL_RESULT" >/dev/null
assert_no_per_approval_rbac "$LIST_APPROVAL_PATH"

OBSERVER_SKILL_PATH="$OBSERVER_PATH/skills/self-created"
OBSERVER_SKILL="$(
  request --fail-with-body --silent --show-error \
    -H "Authorization: Bearer $OBSERVER_TOKEN" \
    -F "bundle=@$SKILL_V1_BUNDLE;type=application/zip" \
    "$SKILL_API/skills?path=$OBSERVER_SKILL_PATH"
)"
jq -e --arg path "$OBSERVER_SKILL_PATH" '
  .metadata.path == $path and .metadata.manifest == "/manifests/skill"
' <<<"$OBSERVER_SKILL" >/dev/null
wait_for_state "$OBSERVER_SKILL_PATH" available >/dev/null
jq -e --arg owner "$OBSERVER_PATH" '.spec.source == $owner' \
  <<<"$(get_resource "$OBSERVER_SKILL_PATH/links/owner")" >/dev/null
CROSS_AGENT_SKILL_STATUS="$(
  command curl --silent --output "$E2E_DIR/cross-agent-skill.json" --write-out "%{http_code}" \
    -H "Authorization: Bearer $OBSERVER_TOKEN" \
    -F "bundle=@$SKILL_V1_BUNDLE;type=application/zip" \
    "$SKILL_API/skills?path=$AGENT_PATH/skills/forbidden"
)"
[[ "$CROSS_AGENT_SKILL_STATUS" == "403" ]]
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
    --arg body "@e2e Use \$e2e-bundle and remember $SESSION_SECRET. Download the attached File using the provided KAS_FILE_API command and read the downloaded bytes. Then use curl with \$KAS_API and \$KAS_TOKEN to POST a Message Resource at $PROOF_PATH with name e2e-agent-network-proof and spec.role system. Set spec.body to the actual exact text you read from the downloaded file. Also POST a Message Resource at $SKILL_PROOF_PATH with name e2e-agent-skill-proof, spec.role system, and spec.body set to the exact Skill bundle marker supplied by \$e2e-bundle. After both POST requests succeed, read both Resources back and verify their bodies, then publish the assistant reply CREATED through the KAS API exactly as required by \$kas. Do not rely on your final terminal response." '{
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
SKILL_PROOF="$(get_resource "$SKILL_PROOF_PATH")"
jq -e '
  .metadata.manifest == "/manifests/message"
  and .spec == {role: "system", body: "KAS_SKILL_BUNDLE_V1"}
' <<<"$SKILL_PROOF" >/dev/null

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

SKILL_BEFORE_UPDATE="$(get_resource "$SKILL_PATH")"
SKILL_REVISION="$(jq -r '.metadata["[kas]"].revision' <<<"$SKILL_BEFORE_UPDATE")"
UPDATED_SKILL="$(
  request --fail-with-body --silent --show-error --request PATCH \
    -H "Authorization: Bearer $ADMIN_TOKEN" \
    -F "bundle=@$SKILL_V2_BUNDLE;type=application/zip" \
    "$SKILL_API/skills?path=$SKILL_PATH&expected_revision=$SKILL_REVISION"
)"
jq -e --arg path "$SKILL_PATH" '
  .metadata.path == $path and .spec.name == "e2e-bundle"
' <<<"$UPDATED_SKILL" >/dev/null
wait_for_state "$SKILL_PATH" available >/dev/null
UPDATED_SKILL_LINK="$(get_resource "$SKILL_PATH/links/bundle")"
UPDATED_SKILL_FILE="$(jq -r '.spec.target' <<<"$UPDATED_SKILL_LINK")"
[[ "$UPDATED_SKILL_FILE" != "$INITIAL_SKILL_FILE" ]]
[[ "$(jq -r '.metadata.path' <<<"$UPDATED_SKILL_LINK")" == "$SKILL_PATH/links/bundle" ]]
get_resource "$UPDATED_SKILL_FILE" >/dev/null

SECOND_MESSAGE_PATH="/messages/e2e-user-resume"
UPDATED_SKILL_PROOF_PATH="/messages/e2e-agent-skill-update-proof"
post_resource "$(
  jq -n \
    --arg path "$SECOND_MESSAGE_PATH" \
    --arg body "@e2e Use \$e2e-bundle and POST a Message Resource at $UPDATED_SKILL_PROOF_PATH with name e2e-agent-skill-update-proof, spec.role system, and spec.body set to its exact current Skill bundle marker. Then publish an assistant reply through the KAS API containing exactly the secret I asked you to remember in the previous turn; do not add the marker to the reply and do not rely on your final terminal response." '{
      metadata: {
        path: $path,
        manifest: "/manifests/message",
        name: "e2e-user-resume"
      },
      spec: {
        role: "user",
        body: $body
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
UPDATED_SKILL_PROOF="$(get_resource "$UPDATED_SKILL_PROOF_PATH")"
jq -e '
  .metadata.manifest == "/manifests/message"
  and .spec == {role: "system", body: "KAS_SKILL_BUNDLE_V2"}
' <<<"$UPDATED_SKILL_PROOF" >/dev/null

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

for _ in $(seq 1 200); do
  ACTIVE_DELIVERIES="$(sqlite3 "$KAS_DATABASE" 'SELECT count(*) FROM driver_deliveries')"
  if [[ "$ACTIVE_DELIVERIES" == "0" ]]; then
    break
  fi
  sleep 0.05
done
[[ "$ACTIVE_DELIVERIES" == "0" ]]
SKILL_EVENT_COUNT="$(
  sqlite3 "$KAS_DATABASE" \
    "SELECT count(*) FROM events WHERE resource_path IN ('$SKILL_PATH','/skills/kas')"
)"
if (( SKILL_EVENT_COUNT >= 50 )); then
  echo "Skill reconciliation did not converge: $SKILL_EVENT_COUNT Skill events" >&2
  false
fi

control_driver "/manifests/message/driver" stopped
control_driver "/manifests/agent/driver" stopped
control_driver "/manifests/skill/driver" stopped
control_driver "/manifests/file/driver" stopped
control_driver "/manifests/approval/driver" stopped

echo "KAS platform Approval, Skill, File, and persistent Session end-to-end test passed"
