#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
FORGE_ROOT="$ROOT/forge"
TEST_DIR="$(mktemp -d "${TMPDIR:-/tmp}/kas-forge-e2e.XXXXXX")"
BASE_PORT="${KAS_FORGE_E2E_BASE_PORT:-$((32000 + ($$ % 1000) * 3))}"
API_PORT="$BASE_PORT"
PACKAGE_PORT="$((BASE_PORT + 1))"
FRONTEND_PORT="$((BASE_PORT + 2))"
API="http://127.0.0.1:$API_PORT"
PACKAGE_API="http://127.0.0.1:$PACKAGE_PORT"
PREVIEW_LOG="$TEST_DIR/preview.log"
PREVIEW_PID=""

cleanup() {
  local status=$?
  trap - EXIT INT TERM
  if [[ -n "$PREVIEW_PID" ]] && kill -0 "$PREVIEW_PID" 2>/dev/null; then
    kill "$PREVIEW_PID" 2>/dev/null || true
    wait "$PREVIEW_PID" 2>/dev/null || true
  fi
  if ((status != 0)); then
    echo "Forge end-to-end test failed (exit $status)." >&2
    [[ -s "$PREVIEW_LOG" ]] && tail -n 160 "$PREVIEW_LOG" >&2
  fi
  if [[ "${KAS_KEEP_E2E:-false}" != "true" && "$TEST_DIR" == "${TMPDIR:-/tmp}"/kas-forge-e2e.* ]]; then
    rm -rf "$TEST_DIR"
  else
    echo "E2E data retained at $TEST_DIR" >&2
  fi
  exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT TERM

for command in codex curl jq uuidgen; do
  command -v "$command" >/dev/null || { echo "missing required command: $command" >&2; exit 1; }
done

echo "Starting isolated Forge preview..."
KAS_FORGE_SEED_REQUEST=false \
KAS_KEEP_PREVIEW=false \
KAS_PREVIEW_API_PORT="$API_PORT" \
KAS_PREVIEW_PACKAGE_PORT="$PACKAGE_PORT" \
KAS_PREVIEW_FRONTEND_PORT="$FRONTEND_PORT" \
  "$FORGE_ROOT/scripts/preview.sh" >"$PREVIEW_LOG" 2>&1 &
PREVIEW_PID="$!"

for _ in $(seq 1 1200); do
  grep -q "KAS Forge preview is ready" "$PREVIEW_LOG" && break
  kill -0 "$PREVIEW_PID" 2>/dev/null || {
    echo "Forge preview stopped before becoming ready" >&2
    false
  }
  sleep 0.1
done
grep -q "KAS Forge preview is ready" "$PREVIEW_LOG"

ADMIN_TOKEN="$(sed -n 's#^Frontend:.*[?]token=##p' "$PREVIEW_LOG" | tail -n 1)"
PREVIEW_DIR="$(sed -n 's#^Logs:        ##p' "$PREVIEW_LOG" | tail -n 1)"
[[ -n "$ADMIN_TOKEN" ]]
[[ -f "$PREVIEW_DIR/packages/hello.kas" ]]
AGENT_PATH="/packages/forge/agent/agents/preview"
AGENT_SERVICE_ACCOUNT="/packages/forge/agent/service-accounts/preview"
HELLO_PACKAGE="/packages/demo/hello"

get_resource() {
  curl --fail-with-body --silent --show-error --get \
    -H "Authorization: Bearer $ADMIN_TOKEN" \
    --data-urlencode "path=$1" \
    "$API/resources/by-path"
}

echo "Checking Agent identity and least-privilege boundary..."
get_resource "$AGENT_PATH" | jq -e '
  .metadata.manifest == "/packages/forge/agent/manifest"
  and .status.metadata.state == "available"
' >/dev/null
get_resource "$AGENT_SERVICE_ACCOUNT" | jq -e '
  .metadata.manifest == "/packages/kas/service-account/manifest"
' >/dev/null

AGENT_TOKEN="$(
  curl --fail-with-body --silent --show-error \
    -H "Authorization: Bearer $ADMIN_TOKEN" \
    -H "Content-Type: application/json" \
    -d "$(jq -cn --arg subject "$AGENT_SERVICE_ACCOUNT" '{subject:$subject}')" \
    "$API/credentials/issue" |
    jq -r '.token'
)"
[[ -n "$AGENT_TOKEN" && "$AGENT_TOKEN" != "null" ]]

curl --fail-with-body --silent --show-error \
  -H "Authorization: Bearer $AGENT_TOKEN" \
  -H "Content-Type: application/json" \
  -d "$(jq -cn --arg path "$HELLO_PACKAGE" '{manifest:"/packages/kas/package/manifest",verb:"create",path:$path}')" \
  "$API/auth/check" |
  jq -e --arg subject "$AGENT_SERVICE_ACCOUNT" '
    .allowed == false and .subject.path == $subject
  ' >/dev/null

echo "Submitting a real Package archive as the Agent..."
REQUEST="$(
  curl --fail-with-body --silent --show-error \
    -X POST \
    -H "Authorization: Bearer $AGENT_TOKEN" \
    -H "Content-Type: application/vnd.kas.manifest+tar" \
    -H "X-KAS-Reason: Add the greeting capability required by the Forge E2E scenario." \
    --data-binary "@$PREVIEW_DIR/packages/hello.kas" \
    "$PACKAGE_API/package-requests"
)"
REQUEST_PATH="$(jq -r '.path' <<<"$REQUEST")"
REQUEST_REVISION="$(jq -r '.metadata["[kas]"].revision' <<<"$REQUEST")"
jq -e '
  .metadata.manifest == "/packages/forge/package-request/manifest"
  and .metadata.state == "pending"
  and .spec.package_path == "/packages/demo/hello"
  and .spec.manifest_path == "/packages/demo/hello/manifest"
  and .spec.reason == "Add the greeting capability required by the Forge E2E scenario."
' <<<"$REQUEST" >/dev/null

HTTP_STATUS="$(
  curl --silent --output "$TEST_DIR/not-installed.json" --write-out '%{http_code}' --get \
    -H "Authorization: Bearer $ADMIN_TOKEN" \
    --data-urlencode "path=$HELLO_PACKAGE" \
    "$API/resources/by-path"
)"
[[ "$HTTP_STATUS" == "404" ]]

get_resource "$REQUEST_PATH/links/requested-by" | jq -e \
  --arg request "$REQUEST_PATH" --arg agent "$AGENT_SERVICE_ACCOUNT" '
  .spec.relation == "/packages/forge/package-request/relations/requested-by"
  and .spec.source == $request
  and .spec.target == $agent
' >/dev/null

echo "Approving with the User credential and installing the Package..."
DECIDED="$(
  curl --fail-with-body --silent --show-error \
    -X POST \
    -H "Authorization: Bearer $ADMIN_TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"decision":"approve"}' \
    "$PACKAGE_API/package-requests/decide?path=$(jq -rn --arg value "$REQUEST_PATH" '$value|@uri')&expected_revision=$REQUEST_REVISION"
)"
jq -e '
  .metadata.state == "installed"
  and .spec.decision.outcome == "installed"
  and .spec.decision.approver == "/packages/kas/user/users/preview-admin"
' <<<"$DECIDED" >/dev/null
get_resource "$HELLO_PACKAGE" | jq -e '
  .metadata.manifest == "/packages/kas/package/manifest"
  and .spec.manifest == "/packages/demo/hello/manifest"
' >/dev/null
get_resource "$REQUEST_PATH/links/decided-by" | jq -e \
  --arg request "$REQUEST_PATH" '
  .spec.relation == "/packages/forge/package-request/relations/decided-by"
  and .spec.source == $request
  and .spec.target == "/packages/kas/user/users/preview-admin"
' >/dev/null

echo "Running the real Codex Agent through Action/Run..."
RUN_ID="$(uuidgen | tr '[:upper:]' '[:lower:]')"
RUN_CREATED="$(curl --fail-with-body --silent --show-error \
  -H "Authorization: Bearer $ADMIN_TOKEN" \
  -H "Content-Type: application/json" \
  -d "$(jq -cn --arg id "$RUN_ID" --arg agent "$AGENT_PATH" '{request_id:$id,resource:$agent,action:"/packages/forge/agent/actions/run",input:{prompt:"Do not modify files. Reply with exactly: Forge Agent is ready"}}')" \
  "$API/runs")"
RUN_PATH="$(jq -r '.path' <<<"$RUN_CREATED")"
jq -e --arg id "$RUN_ID" '
  (.path | startswith("/packages/kas/run/runs/"))
  and .metadata["[kas]"].protected == true
  and .spec.request_id == $id
  and .spec.subject == "/packages/kas/user/users/preview-admin"
' <<<"$RUN_CREATED" >/dev/null

RUN=""
for _ in $(seq 1 1200); do
  RUN="$(get_resource "$RUN_PATH")"
  case "$(jq -r '.metadata.state' <<<"$RUN")" in
    succeeded|failed) break ;;
  esac
  sleep 0.1
done
jq -e '
  .metadata.state == "succeeded"
  and .spec.output.response == "Forge Agent is ready"
' <<<"$RUN" >/dev/null

echo "Forge end-to-end test passed."
