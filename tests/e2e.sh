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
cargo build --workspace

PORT="$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1]); s.close()')"
API="http://127.0.0.1:$PORT"
export KAS_DATA_DIR="$E2E_DIR/data"
export KAS_DATABASE="$KAS_DATA_DIR/kas.db"
export KAS_ADDRESS="127.0.0.1:$PORT"
export KAS_API_URL="$API"

target/debug/kas-migrate
ADMIN_TOKEN="$(target/debug/kas-admin bootstrap e2e-admin)"

target/debug/kas-api >"$API_LOG" 2>&1 &
API_PID="$!"

for _ in $(seq 1 100); do
  if curl --fail --silent "$API/health" >/dev/null; then
    break
  fi
  sleep 0.05
done
curl --fail --silent "$API/health" | jq -e '.ok == true' >/dev/null

curl --fail --silent \
  -H "Authorization: Bearer $ADMIN_TOKEN" \
  "$API/manifests" |
  jq -e '
    ([.[].path] | sort) == ([
      "/manifests/system/auth",
      "/manifests/system/core"
    ] | sort)
  ' >/dev/null

PACKAGE_ROOT="$E2E_DIR/package"
mkdir -p "$PACKAGE_ROOT/driver/bin"
cp tests/fixtures/echo/manifest.json "$PACKAGE_ROOT/manifest.json"
cp target/debug/kas-test-driver "$PACKAGE_ROOT/driver/bin/kas-test-driver"
chmod 755 "$PACKAGE_ROOT/driver/bin/kas-test-driver"
tar -C "$PACKAGE_ROOT" -cf "$E2E_DIR/echo.kas" manifest.json driver

MANIFEST="$(
  curl --silent --show-error \
    -H "Authorization: Bearer $ADMIN_TOKEN" \
    -H "Content-Type: application/vnd.kas.manifest+tar" \
    --data-binary "@$E2E_DIR/echo.kas" \
    "$API/manifests"
)"
echo "$MANIFEST" | jq -e '
  .path == "/manifests/echo"
  and .driver.path == "/manifests/echo/driver"
  and (.package_digest | startswith("sha256:"))
' >/dev/null || {
  echo "Manifest installation failed: $MANIFEST" >&2
  false
}

curl --fail --silent --get \
  -H "Authorization: Bearer $ADMIN_TOKEN" \
  --data-urlencode "source_kind=manifest" \
  --data-urlencode "source_path=/manifests/echo" \
  "$API/links" |
  jq -e '
    length == 5
    and ([.[].relation_path] | unique) == [
      "/manifests/system/core/relations/manifest-member"
    ]
    and ([.[].target.kind] | sort) == ([
      "action",
      "driver",
      "role",
      "role_binding",
      "service_account"
    ] | sort)
  ' >/dev/null

DRIVER_PATH="/manifests/echo/driver"
DRIVER_URL="$API/drivers/by-path?path=$(jq -rn --arg value "$DRIVER_PATH" '$value | @uri')"

curl --fail --silent --get \
  -H "Authorization: Bearer $ADMIN_TOKEN" \
  --data-urlencode "source_kind=driver" \
  --data-urlencode "source_path=$DRIVER_PATH" \
  "$API/links" |
  jq -e '
    length == 1
    and .[0].relation_path == "/manifests/system/core/relations/driver-service-account"
    and .[0].target.kind == "service_account"
    and .[0].target.path == "/manifests/echo/service-accounts/driver"
  ' >/dev/null

curl --fail --silent --get \
  -H "Authorization: Bearer $ADMIN_TOKEN" \
  --data-urlencode "source_kind=role_binding" \
  --data-urlencode "source_path=/manifests/echo/role-bindings/driver" \
  "$API/links" |
  jq -e '
    length == 2
    and ([.[].relation_path] | sort) == ([
      "/manifests/system/auth/relations/role-binding-role",
      "/manifests/system/auth/relations/role-binding-subject"
    ] | sort)
    and ([.[].target.kind] | sort) == ([
      "role",
      "service_account"
    ] | sort)
  ' >/dev/null

DRIVER=""
for _ in $(seq 1 200); do
  DRIVER="$(
    curl --fail --silent --get \
      -H "Authorization: Bearer $ADMIN_TOKEN" \
      --data-urlencode "path=/manifests/echo" \
      "$API/manifests/driver"
  )"
  if [[ "$(echo "$DRIVER" | jq -r '.state')" == "ready" ]]; then
    break
  fi
  sleep 0.05
done
echo "$DRIVER" | jq -e '
  .path == "/manifests/echo/driver"
  and .desired_state == "running"
  and .state == "ready"
  and .process_id != null
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
  jq -e '.path == "/resources/e2e/echo"' >/dev/null

curl --fail --silent --get \
  -H "Authorization: Bearer $ADMIN_TOKEN" \
  --data-urlencode "path=$RESOURCE_PATH" \
  --data-urlencode "include=relations" \
  "$API/resources/by-path" |
  jq -e '
    .path == "/resources/e2e/echo"
    and (.links | length) == 1
    and .links[0].relation_path == "/manifests/system/core/relations/resource-manifest"
    and (.related | length) == 1
    and .related[0].kind == "manifest"
    and .related[0].value.path == "/manifests/echo"
  ' >/dev/null

REQUEST_ID="$(uuidgen | tr '[:upper:]' '[:lower:]')"
RUN_PATH="$RESOURCE_PATH/runs/$REQUEST_ID"
RUN_PAYLOAD="$(
  jq -n \
    --arg run "$RUN_PATH" \
    --arg request_id "$REQUEST_ID" \
    --arg resource "$RESOURCE_PATH" '{
      path: $run,
      request_id: $request_id,
      resource: $resource,
      action: "/manifests/echo/actions/echo",
      input: {message: "hello from e2e"},
      links: []
    }'
)"
curl --fail --silent \
  -H "Authorization: Bearer $ADMIN_TOKEN" \
  -H "Content-Type: application/json" \
  -d "$RUN_PAYLOAD" \
  "$API/runs" |
  jq -e '.status == "queued"' >/dev/null

RUN=""
for _ in $(seq 1 200); do
  RUN="$(
    curl --fail --silent --get \
      -H "Authorization: Bearer $ADMIN_TOKEN" \
      --data-urlencode "path=$RUN_PATH" \
      "$API/runs/by-path"
  )"
  if [[ "$(echo "$RUN" | jq -r '.status')" == "succeeded" ]]; then
    break
  fi
  sleep 0.05
done
echo "$RUN" | jq -e '
  .status == "succeeded"
  and .output.echo.message == "hello from e2e"
  and .driver_generation == 1
' >/dev/null

curl --fail --silent --get \
  -H "Authorization: Bearer $ADMIN_TOKEN" \
  --data-urlencode "source_kind=run" \
  --data-urlencode "source_path=$RUN_PATH" \
  "$API/links" |
  jq -e '
    length == 3
    and ([.[].relation_path] | sort) == ([
      "/manifests/system/core/relations/run-action",
      "/manifests/system/core/relations/run-driver",
      "/manifests/system/core/relations/run-resource"
    ] | sort)
  ' >/dev/null

curl --fail --silent --request PATCH \
  -H "Authorization: Bearer $ADMIN_TOKEN" \
  -H "Content-Type: application/json" \
  --data '{"state":"stopping"}' \
  "$DRIVER_URL" >/dev/null

for _ in $(seq 1 100); do
  DRIVER="$(
    curl --fail --silent --get \
      -H "Authorization: Bearer $ADMIN_TOKEN" \
      --data-urlencode "path=$DRIVER_PATH" \
      "$API/drivers/by-path"
  )"
  if [[ "$(echo "$DRIVER" | jq -r '.state')" == "stopped" ]]; then
    break
  fi
  sleep 0.05
done
echo "$DRIVER" | jq -e '
  .desired_state == "stopped"
  and .state == "stopped"
  and .process_id == null
' >/dev/null

echo "KAS end-to-end test passed"
