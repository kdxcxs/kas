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

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
E2E_DIR="$(mktemp -d "${TMPDIR:-/tmp}/kas-e2e.XXXXXX")"
API_PID=""
API_LOG="$E2E_DIR/kas-api.log"

cleanup() {
  if [[ -n "$API_PID" ]] && kill -0 "$API_PID" 2>/dev/null; then
    kill "$API_PID" 2>/dev/null || true
    wait "$API_PID" 2>/dev/null || true
  fi
  rm -rf "$E2E_DIR"
}

failed() {
  local line="$1"
  echo "E2E failed at line $line" >&2
  if [[ -f "$API_LOG" ]]; then
    echo "kas-api output:" >&2
    sed -n '1,240p' "$API_LOG" >&2
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

curl() {
  if [[ "$VERBOSE" != true ]]; then
    command curl "$@"
    return
  fi

  local argument
  local body
  local capture_body=0
  local capture_method=0
  local displayed
  local explicit_method=0
  local method="GET"
  local response
  local status
  local url="<unknown>"
  local -a bodies=()

  for argument in "$@"; do
    if ((capture_method)); then
      method="$argument"
      explicit_method=1
      capture_method=0
      continue
    fi
    if ((capture_body)); then
      bodies+=("$argument")
      capture_body=0
      if ((explicit_method == 0)); then
        method="POST"
      fi
      continue
    fi
    case "$argument" in
      --request | -X)
        capture_method=1
        ;;
      --request=*)
        method="${argument#*=}"
        explicit_method=1
        ;;
      --get | -G)
        method="GET"
        explicit_method=1
        ;;
      --data | --data-raw | --data-binary | -d)
        capture_body=1
        ;;
      --data=* | --data-raw=* | --data-binary=*)
        bodies+=("${argument#*=}")
        if ((explicit_method == 0)); then
          method="POST"
        fi
        ;;
      http://* | https://*)
        url="$argument"
        ;;
    esac
  done

  printf '\n>>> Request: %s %s\n' "$method" "$url" >&2
  printf '>>> Command: curl' >&2
  for argument in "$@"; do
    displayed="$argument"
    if [[ "$displayed" == "Authorization: Bearer "* ]]; then
      displayed="Authorization: Bearer <redacted>"
    fi
    printf ' %q' "$displayed" >&2
  done
  printf '\n' >&2

  if ((${#bodies[@]} > 0)); then
    for body in "${bodies[@]}"; do
      printf '>>> Request body:\n' >&2
      if [[ "$body" == @* ]]; then
        printf '%s\n' "$body" >&2
      elif jq -e . >/dev/null 2>&1 <<<"$body"; then
        jq . <<<"$body" >&2
      else
        printf '%s\n' "$body" >&2
      fi
    done
  fi

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
cargo build -j 1 -p kas-api -p kas-migrate -p kas-admin -p kas-test-driver

PORT="$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1]); s.close()')"
API="http://127.0.0.1:$PORT"
export KAS_DATA_DIR="$E2E_DIR/data"
export KAS_DATABASE="$KAS_DATA_DIR/kas.db"
export KAS_ADDRESS="127.0.0.1:$PORT"
export KAS_API_URL="$API"

"$TARGET_DIR/debug/kas-migrate"
ADMIN_TOKEN="$("$TARGET_DIR/debug/kas-admin" bootstrap e2e-admin)"

"$TARGET_DIR/debug/kas-api" >"$API_LOG" 2>&1 &
API_PID="$!"

for _ in $(seq 1 100); do
  if curl --fail --silent "$API/health" >/dev/null; then
    break
  fi
  sleep 0.05
done
curl --fail --silent "$API/health" | jq -e '.ok == true' >/dev/null

curl --fail --silent --get \
  -H "Authorization: Bearer $ADMIN_TOKEN" \
  --data-urlencode "path=/builtin/manifest" \
  "$API/resources/by-path" |
  jq -e '
    .path == "/builtin/manifest"
    and .manifest == "/builtin/manifest"
    and .spec.version == 1
  ' >/dev/null

curl --fail --silent --get \
  -H "Authorization: Bearer $ADMIN_TOKEN" \
  --data-urlencode "manifest=/builtin/manifest" \
  "$API/resources" |
  jq -e '
    ([.[].path] | index("/builtin/manifest")) != null
    and ([.[].path] | index("/builtin/driver")) != null
    and ([.[].path] | index("/builtin/run")) != null
    and ([.[].path] | index("/builtin/link")) != null
  ' >/dev/null

PACKAGE_ROOT="$E2E_DIR/package"
mkdir -p "$PACKAGE_ROOT/driver/bin"
cp tests/fixtures/echo/manifest.json "$PACKAGE_ROOT/manifest.json"
cp -R tests/fixtures/echo/resources "$PACKAGE_ROOT/resources"
cp "$TARGET_DIR/debug/kas-test-driver" "$PACKAGE_ROOT/driver/bin/kas-test-driver"
chmod 755 "$PACKAGE_ROOT/driver/bin/kas-test-driver"
COPYFILE_DISABLE=1 tar -C "$PACKAGE_ROOT" -cf "$E2E_DIR/echo.kas" manifest.json resources driver

PACKAGE="$(
  curl --silent --show-error \
    -H "Authorization: Bearer $ADMIN_TOKEN" \
    -H "Content-Type: application/vnd.kas.manifest+tar" \
    --data-binary "@$E2E_DIR/echo.kas" \
    "$API/packages"
)"
echo "$PACKAGE" | jq -e '
  (.path | startswith("/packages/sha256/"))
  and .manifest == "/builtin/package"
  and (.spec.digest | startswith("sha256:"))
  and .spec.size_bytes > 0
  and .spec.media_type == "application/vnd.kas.manifest+tar"
  and .spec == .status
' >/dev/null || {
  echo "Package installation failed: $PACKAGE" >&2
  false
}
PACKAGE_PATH="$(echo "$PACKAGE" | jq -r '.path')"

curl --fail --silent --get \
  -H "Authorization: Bearer $ADMIN_TOKEN" \
  --data-urlencode "path=/manifests/echo" \
  "$API/resources/by-path" |
  jq -e '
    .path == "/manifests/echo"
    and .manifest == "/builtin/manifest"
    and (.spec | has("package_digest") | not)
  ' >/dev/null

curl --fail --silent --get \
  -H "Authorization: Bearer $ADMIN_TOKEN" \
  --data-urlencode "manifest=/builtin/link" \
  "$API/resources" |
  jq -e '
    [.[] | select(
      .spec.source == "/manifests/echo"
      and (.spec.target | startswith("/manifests/echo/"))
    )] | length == 6
  ' >/dev/null

curl --fail --silent --get \
  -H "Authorization: Bearer $ADMIN_TOKEN" \
  --data-urlencode "manifest=/builtin/link" \
  "$API/resources" |
  jq -e --arg package "$PACKAGE_PATH" '
    [.[] | select(
      .spec.relation == "/builtin/relations/package-manifest"
      and .spec.source == $package
      and .spec.target == "/manifests/echo"
    )] | length == 1
  ' >/dev/null

DRIVER_PATH="/manifests/echo/driver"

DRIVER=""
for _ in $(seq 1 200); do
  DRIVER="$(
    curl --fail --silent --get \
      -H "Authorization: Bearer $ADMIN_TOKEN" \
      --data-urlencode "path=$DRIVER_PATH" \
      "$API/resources/by-path"
  )"
  if [[ "$(echo "$DRIVER" | jq -r '.status.state')" == "ready" ]]; then
    break
  fi
  sleep 0.05
done
echo "$DRIVER" | jq -e '
  .path == "/manifests/echo/driver"
  and .manifest == "/builtin/driver"
  and .status.desired_state == "running"
  and .status.state == "ready"
  and .status.process_id != null
' >/dev/null

RESOURCE_PATH="/resources/e2e/echo"
RESOURCE_PAYLOAD="$(
  jq -n --arg resource "$RESOURCE_PATH" '{
    path: $resource,
    manifest: "/manifests/echo",
    name: "echo",
    spec: {label: "fixture"}
  }'
)"
curl --fail --silent \
  -H "Authorization: Bearer $ADMIN_TOKEN" \
  -H "Content-Type: application/json" \
  -d "$RESOURCE_PAYLOAD" \
  "$API/resources" |
  jq -e '
    .path == "/resources/e2e/echo"
    and .spec.state == "available"
    and .status.state == "pending"
  ' >/dev/null

RESOURCE=""
for _ in $(seq 1 200); do
  RESOURCE="$(
    curl --fail --silent --get \
      -H "Authorization: Bearer $ADMIN_TOKEN" \
      --data-urlencode "path=$RESOURCE_PATH" \
      "$API/resources/by-path"
  )"
  if [[ "$(echo "$RESOURCE" | jq -r '.status.state')" == "available" ]]; then
    break
  fi
  sleep 0.05
done
echo "$RESOURCE" | jq -e '.spec == .status and .status.state == "available"' >/dev/null

LINK_PATH="/links/e2e/echo-self"
curl --fail --silent \
  -H "Authorization: Bearer $ADMIN_TOKEN" \
  -H "Content-Type: application/json" \
  -d "$(
    jq -n --arg path "$LINK_PATH" --arg resource "$RESOURCE_PATH" '{
      path: $path,
      manifest: "/builtin/link",
      name: "echo-self",
      spec: {
        relation: "/manifests/echo/relations/peer",
        source: $resource,
        target: $resource,
        metadata: {}
      }
    }'
  )" \
  "$API/resources" >/dev/null
LINK=""
for _ in $(seq 1 200); do
  LINK="$(
    curl --fail --silent --get \
      -H "Authorization: Bearer $ADMIN_TOKEN" \
      --data-urlencode "path=$LINK_PATH" \
      "$API/resources/by-path"
  )"
  if [[ "$(echo "$LINK" | jq -r '.status.relation // empty')" == "/manifests/echo/relations/peer" ]]; then
    break
  fi
  sleep 0.05
done
echo "$LINK" | jq -e '
  .manifest == "/builtin/link"
  and .spec == .status
' >/dev/null

REQUEST_ID="$(uuidgen | tr '[:upper:]' '[:lower:]')"
RUN_PATH="$RESOURCE_PATH/runs/$REQUEST_ID"
RUN_PAYLOAD="$(
  jq -n \
    --arg run "$RUN_PATH" \
    --arg request_id "$REQUEST_ID" \
    --arg resource "$RESOURCE_PATH" '{
      path: $run,
      manifest: "/builtin/run",
      name: $request_id,
      spec: {
        request_id: $request_id,
        resource: $resource,
        action: "/manifests/echo/actions/echo",
        input: {message: "hello from e2e"}
      }
    }'
)"
curl --fail --silent \
  -H "Authorization: Bearer $ADMIN_TOKEN" \
  -H "Content-Type: application/json" \
  -d "$RUN_PAYLOAD" \
  "$API/resources" |
  jq -e '.status.state == "queued"' >/dev/null

RUN=""
for _ in $(seq 1 200); do
  RUN="$(
    curl --fail --silent --get \
      -H "Authorization: Bearer $ADMIN_TOKEN" \
      --data-urlencode "path=$RUN_PATH" \
      "$API/resources/by-path"
  )"
  if [[ "$(echo "$RUN" | jq -r '.status.state')" == "succeeded" ]]; then
    break
  fi
  sleep 0.05
done
echo "$RUN" | jq -e '
  .status.state == "succeeded"
  and .status.output.echo.message == "hello from e2e"
  and .status.driver_generation == 1
' >/dev/null

curl --fail --silent --get \
  -H "Authorization: Bearer $ADMIN_TOKEN" \
  --data-urlencode "manifest=/builtin/link" \
  "$API/resources" |
  jq -e --arg run "$RUN_PATH" '
    [.[] | select(.spec.source == $run)] | length == 3
  ' >/dev/null

RESOURCE_REVISION="$(
  curl --fail --silent --get \
    -H "Authorization: Bearer $ADMIN_TOKEN" \
    --data-urlencode "path=$RESOURCE_PATH" \
    "$API/resources/by-path" |
    jq -r '.revision'
)"
curl --fail --silent --request DELETE \
  -H "Authorization: Bearer $ADMIN_TOKEN" \
  --get \
  --data-urlencode "path=$RESOURCE_PATH" \
  --data-urlencode "expected_revision=$RESOURCE_REVISION" \
  "$API/resources/by-path" |
  jq -e '.spec.state == "deleted"' >/dev/null

for _ in $(seq 1 200); do
  RESOURCE_STATUS="$(
    curl --silent --output "$E2E_DIR/deleted-resource.json" --write-out "%{http_code}" \
      --get \
      -H "Authorization: Bearer $ADMIN_TOKEN" \
      --data-urlencode "path=$RESOURCE_PATH" \
      "$API/resources/by-path"
  )"
  if [[ "$RESOURCE_STATUS" == "404" ]]; then
    break
  fi
  sleep 0.05
done
[[ "$RESOURCE_STATUS" == "404" ]]

curl --fail --silent \
  -H "Authorization: Bearer $ADMIN_TOKEN" \
  -H "Content-Type: application/json" \
  -d "$RESOURCE_PAYLOAD" \
  "$API/resources" |
  jq -e '
    .path == "/resources/e2e/echo"
    and .revision == 0
    and .status.state == "pending"
  ' >/dev/null

curl --fail --silent \
  -H "Authorization: Bearer $ADMIN_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"path":"/users/e2e-viewer","manifest":"/builtin/user","name":"e2e-viewer","spec":{"disabled":false}}' \
  "$API/resources" >/dev/null
curl --fail --silent \
  -H "Authorization: Bearer $ADMIN_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"path":"/roles/e2e-viewer","manifest":"/builtin/role","name":"e2e-viewer","spec":{"rules":[{"manifests":["/manifests/echo"],"verbs":["get"],"paths":["/resources/e2e/**"]}]}}' \
  "$API/resources" >/dev/null
curl --fail --silent \
  -H "Authorization: Bearer $ADMIN_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"path":"/role-bindings/e2e-viewer","manifest":"/builtin/role-binding","name":"e2e-viewer","spec":{"role":"/roles/e2e-viewer","subjects":["/users/e2e-viewer"]}}' \
  "$API/resources" >/dev/null
VIEWER_TOKEN="$(
  curl --fail --silent \
    -H "Authorization: Bearer $ADMIN_TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"subject":"/users/e2e-viewer"}' \
    "$API/credentials/issue" |
    jq -r '.token'
)"
curl --fail --silent --get \
  -H "Authorization: Bearer $VIEWER_TOKEN" \
  --data-urlencode "path=$RESOURCE_PATH" \
  "$API/resources/by-path" |
  jq -e '.path == "/resources/e2e/echo"' >/dev/null
RBAC_STATUS="$(
  curl --silent --output "$E2E_DIR/rbac-denied.json" --write-out "%{http_code}" \
    --get \
    -H "Authorization: Bearer $VIEWER_TOKEN" \
    --data-urlencode "path=$DRIVER_PATH" \
    "$API/resources/by-path"
)"
[[ "$RBAC_STATUS" == "403" ]]

curl --fail --silent \
  -H "Authorization: Bearer $ADMIN_TOKEN" \
  -H "Content-Type: application/json" \
  -d "{\"path\":\"$DRIVER_PATH\",\"desired_state\":\"stopped\"}" \
  "$API/drivers/control" >/dev/null

for _ in $(seq 1 100); do
  DRIVER="$(
    curl --fail --silent --get \
      -H "Authorization: Bearer $ADMIN_TOKEN" \
      --data-urlencode "path=$DRIVER_PATH" \
      "$API/resources/by-path"
  )"
  if [[ "$(echo "$DRIVER" | jq -r '.status.state')" == "stopped" ]]; then
    break
  fi
  sleep 0.05
done
echo "$DRIVER" | jq -e '
  .status.desired_state == "stopped"
  and .status.state == "stopped"
  and .status.process_id == null
' >/dev/null

echo "KAS end-to-end test passed"
