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
    tail -n 240 "$API_LOG" >&2
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
cargo build -j 1 \
  -p kas-api \
  -p kas-builtin-driver \
  -p kas-migrate \
  -p kas-admin \
  -p kas-test-driver

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

BUILTIN_DRIVER_PATH="/builtin/link/driver"
BUILTIN_DRIVER=""
for _ in $(seq 1 200); do
  BUILTIN_DRIVER="$(
    curl --fail --silent --get \
      -H "Authorization: Bearer $ADMIN_TOKEN" \
      --data-urlencode "path=$BUILTIN_DRIVER_PATH" \
      "$API/resources/by-path"
  )"
  if [[ "$(echo "$BUILTIN_DRIVER" | jq -r '.status.metadata.state')" == "running" ]]; then
    break
  fi
  sleep 0.05
done
echo "$BUILTIN_DRIVER" | jq -e '
  .metadata.manifest == "/builtin/driver"
  and .metadata.state == "running"
  and .status.metadata.state == "running"
  and .status.spec == .spec
  and (.spec.manages | sort) == ["/builtin/link", "/builtin/relation"]
' >/dev/null

[[ "$(
  curl --silent --output "$E2E_DIR/removed-relation-driver.json" --write-out "%{http_code}" \
    --get \
    -H "Authorization: Bearer $ADMIN_TOKEN" \
    --data-urlencode "path=/builtin/relation/driver" \
    "$API/resources/by-path"
)" == "404" ]]

curl --fail --silent --get \
  -H "Authorization: Bearer $ADMIN_TOKEN" \
  --data-urlencode "path=/builtin/manifest" \
  "$API/resources/by-path" |
  jq -e '
    .path == "/builtin/manifest"
    and .metadata.manifest == "/builtin/manifest"
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
  and .metadata.manifest == "/builtin/package"
  and (.spec.digest | startswith("sha256:"))
  and .spec.size_bytes > 0
  and .spec.media_type == "application/vnd.kas.manifest+tar"
  and .spec.manifest == "/manifests/echo"
  and .spec.manifest_version == 1
  and .spec == .status.spec
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
    and .metadata.manifest == "/builtin/manifest"
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
  if [[ "$(echo "$DRIVER" | jq -r '.status.metadata.state')" == "running" ]]; then
    break
  fi
  sleep 0.05
done
echo "$DRIVER" | jq -e '
  .path == "/manifests/echo/driver"
  and .metadata.manifest == "/builtin/driver"
  and .metadata.state == "running"
  and .status.metadata.state == "running"
  and .status.spec == .spec
' >/dev/null

DRIVER_SERVICE_ACCOUNT="$(jq -r '.spec.service_account' <<<"$DRIVER")"
DRIVER_GENERATION="$(jq -r '.metadata["[kas]"].generation' <<<"$DRIVER")"
DRIVER_CREDENTIAL_PATH="$(
  curl --fail --silent --get \
    -H "Authorization: Bearer $ADMIN_TOKEN" \
    --data-urlencode "manifest=/builtin/credential" \
    "$API/resources" |
    jq -er \
      --arg subject "$DRIVER_SERVICE_ACCOUNT" \
      --argjson generation "$DRIVER_GENERATION" '
        [
          .[] | select(
            .spec.subject == $subject
            and .spec.driver_generation == $generation
            and (.spec | has("expires_at") | not)
            and (.spec | has("revoked_at") | not)
          )
        ] | if length == 1 then .[0].path else error("expected one active Driver Credential") end
      '
)"
curl --fail --silent --get \
  -H "Authorization: Bearer $ADMIN_TOKEN" \
  --data-urlencode "manifest=/builtin/link" \
  "$API/resources" |
  jq -e \
    --arg driver "$DRIVER_PATH" \
    --arg credential "$DRIVER_CREDENTIAL_PATH" '
      [
        .[] | select(
          .spec.relation == "/builtin/relations/driver-credential"
          and .spec.source == $driver
          and .spec.target == $credential
          and .metadata["[kas]"].protected == true
          and .metadata["[kas]"].managed_by == "system"
        )
      ] | length == 1
    ' >/dev/null

# The single built-in Relationship Driver owns Relation status as well as Link
# status, even though its Driver Resource belongs to the Link package.
PEER_RELATION=""
for _ in $(seq 1 200); do
  PEER_RELATION="$(
    curl --fail --silent --get \
      -H "Authorization: Bearer $ADMIN_TOKEN" \
      --data-urlencode "path=/manifests/echo/relations/peer" \
      "$API/resources/by-path"
  )"
  if [[ "$(echo "$PEER_RELATION" | jq -r '.status.metadata.state')" == "available" ]] &&
     [[ "$(echo "$PEER_RELATION" | jq -r '.status.metadata["[kas]"].observed["/builtin/link/driver"].resource_revision // empty')" == "$(echo "$PEER_RELATION" | jq -r '.metadata["[kas]"].revision')" ]]; then
    break
  fi
  sleep 0.05
done
echo "$PEER_RELATION" | jq -e '
  .metadata.manifest == "/builtin/relation"
  and .status.metadata.state == "available"
  and .status.spec == .spec
  and .metadata["[kas]"].observed["/builtin/link/driver"] == .status.metadata["[kas]"].observed["/builtin/link/driver"]
  and .status.metadata["[kas]"].observed["/builtin/link/driver"].resource_revision == .metadata["[kas]"].revision
' >/dev/null

# A newly registered Driver must backfill Resources that already match its
# watches. The admin User existed before the Echo package was installed.
ADMIN_USER=""
for _ in $(seq 1 200); do
  ADMIN_USER="$(
    curl --fail --silent --get \
      -H "Authorization: Bearer $ADMIN_TOKEN" \
      --data-urlencode "path=/users/e2e-admin" \
      "$API/resources/by-path"
  )"
  if [[ "$(echo "$ADMIN_USER" | jq -r '.status.metadata["[kas]"].observed["/manifests/echo/driver"].resource_revision // empty')" == "$(echo "$ADMIN_USER" | jq -r '.metadata["[kas]"].revision')" ]]; then
    break
  fi
  sleep 0.05
done
echo "$ADMIN_USER" | jq -e '
  .metadata["[kas]"].observed["/manifests/echo/driver"] == .status.metadata["[kas]"].observed["/manifests/echo/driver"]
  and .status.metadata["[kas]"].observed["/manifests/echo/driver"].resource_revision == .metadata["[kas]"].revision
  and .status.metadata["[kas]"].observed["/manifests/echo/driver"].driver_revision == 0
' >/dev/null

# An existing wildcard watch must also include Resources from a Manifest that
# is registered later in the same package transaction.
INTEGRATION_ROOT="$E2E_DIR/integration-package"
mkdir -p "$INTEGRATION_ROOT"
cp -R tests/fixtures/integration/. "$INTEGRATION_ROOT/"
COPYFILE_DISABLE=1 tar -C "$INTEGRATION_ROOT" -cf "$E2E_DIR/integration.kas" manifest.json resources
curl --fail --silent \
  -H "Authorization: Bearer $ADMIN_TOKEN" \
  -H "Content-Type: application/vnd.kas.manifest+tar" \
  --data-binary "@$E2E_DIR/integration.kas" \
  "$API/packages" >/dev/null

INTEGRATION_RESOURCE=""
for _ in $(seq 1 200); do
  INTEGRATION_RESOURCE="$(
    curl --fail --silent --get \
      -H "Authorization: Bearer $ADMIN_TOKEN" \
      --data-urlencode "path=/resources/integrations/demo" \
      "$API/resources/by-path"
  )"
  if [[ "$(echo "$INTEGRATION_RESOURCE" | jq -r '.status.metadata["[kas]"].observed["/manifests/echo/driver"].resource_revision // empty')" == "$(echo "$INTEGRATION_RESOURCE" | jq -r '.metadata["[kas]"].revision')" ]]; then
    break
  fi
  sleep 0.05
done
echo "$INTEGRATION_RESOURCE" | jq -e '
  .metadata.manifest == "/manifests/integration-demo"
  and .metadata["[kas]"].observed["/manifests/echo/driver"] == .status.metadata["[kas]"].observed["/manifests/echo/driver"]
  and .status.metadata["[kas]"].observed["/manifests/echo/driver"].resource_revision == .metadata["[kas]"].revision
' >/dev/null

RESOURCE_PATH="/resources/e2e/echo"
RESOURCE_PAYLOAD="$(
  jq -n --arg resource "$RESOURCE_PATH" '{
    path: $resource,
    metadata: {
      manifest: "/manifests/echo",
      name: "echo"
    },
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
    and .metadata.state == "available"
    and .status.metadata.state == "pending"
  ' >/dev/null

RESOURCE=""
for _ in $(seq 1 200); do
  RESOURCE="$(
    curl --fail --silent --get \
      -H "Authorization: Bearer $ADMIN_TOKEN" \
      --data-urlencode "path=$RESOURCE_PATH" \
      "$API/resources/by-path"
  )"
  if [[ "$(echo "$RESOURCE" | jq -r '.status.metadata.state')" == "available" ]]; then
    break
  fi
  sleep 0.05
done
echo "$RESOURCE" | jq -e --arg package "$PACKAGE_PATH" '
  .spec == .status.spec
  and .status.metadata.state == "available"
  and .metadata["[kas]"].package == $package
  and .status.metadata["[kas]"].package == $package
  and .metadata["[kas]"].observed["/manifests/echo/driver"] == .status.metadata["[kas]"].observed["/manifests/echo/driver"]
  and .status.metadata["[kas]"].observed["/manifests/echo/driver"].resource_revision == .metadata["[kas]"].revision
' >/dev/null

LINK_PATH="/links/e2e/echo-self"
curl --fail --silent \
  -H "Authorization: Bearer $ADMIN_TOKEN" \
  -H "Content-Type: application/json" \
  -d "$(
    jq -n --arg path "$LINK_PATH" --arg resource "$RESOURCE_PATH" '{
      path: $path,
      metadata: {
        manifest: "/builtin/link",
        name: "echo-self"
      },
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
  if [[ "$(echo "$LINK" | jq -r '.status.metadata.state')" == "available" ]] &&
     [[ "$(echo "$LINK" | jq -r '.status.metadata["[kas]"].observed["/builtin/link/driver"].resource_revision // empty')" == "$(echo "$LINK" | jq -r '.metadata["[kas]"].revision')" ]] &&
     [[ "$(echo "$LINK" | jq -r '.status.metadata["[kas]"].observed["/manifests/echo/driver"].resource_revision // empty')" == "$(echo "$LINK" | jq -r '.metadata["[kas]"].revision')" ]]; then
    break
  fi
  sleep 0.05
done
echo "$LINK" | jq -e '
  .metadata.manifest == "/builtin/link"
  and .status.metadata.state == "available"
  and .status.spec == .spec
  and .metadata["[kas]"].observed["/builtin/link/driver"] == .status.metadata["[kas]"].observed["/builtin/link/driver"]
  and .metadata["[kas]"].observed["/manifests/echo/driver"] == .status.metadata["[kas]"].observed["/manifests/echo/driver"]
  and .status.metadata["[kas]"].observed["/builtin/link/driver"].resource_revision == .metadata["[kas]"].revision
  and .status.metadata["[kas]"].observed["/manifests/echo/driver"].resource_revision == .metadata["[kas]"].revision
' >/dev/null || {
  echo "Link reconciliation failed: $LINK" >&2
  false
}

INVALID_LINK_PATH="/links/e2e/invalid-target"
curl --fail --silent \
  -H "Authorization: Bearer $ADMIN_TOKEN" \
  -H "Content-Type: application/json" \
  -d "$(
    jq -n --arg path "$INVALID_LINK_PATH" --arg resource "$RESOURCE_PATH" '{
      path: $path,
      metadata: {
        manifest: "/builtin/link",
        name: "invalid-target"
      },
      spec: {
        relation: "/manifests/echo/relations/peer",
        source: $resource,
        target: "/users/e2e-admin",
        metadata: {}
      }
    }'
  )" \
  "$API/resources" >/dev/null
INVALID_LINK=""
for _ in $(seq 1 200); do
  INVALID_LINK="$(
    curl --fail --silent --get \
      -H "Authorization: Bearer $ADMIN_TOKEN" \
      --data-urlencode "path=$INVALID_LINK_PATH" \
      "$API/resources/by-path"
  )"
  if [[ "$(echo "$INVALID_LINK" | jq -r '.status.metadata.state')" == "invalid" ]] &&
     [[ "$(echo "$INVALID_LINK" | jq -r '.status.metadata["[kas]"].observed["/builtin/link/driver"].resource_revision // empty')" == "$(echo "$INVALID_LINK" | jq -r '.metadata["[kas]"].revision')" ]]; then
    break
  fi
  sleep 0.05
done
echo "$INVALID_LINK" | jq -e '
  .metadata.manifest == "/builtin/link"
  and .status.metadata.state == "invalid"
  and .status.spec == .spec
  and .metadata["[kas]"].observed["/builtin/link/driver"] == .status.metadata["[kas]"].observed["/builtin/link/driver"]
  and .status.metadata["[kas]"].observed["/builtin/link/driver"].resource_revision == .metadata["[kas]"].revision
' >/dev/null || {
  echo "Invalid Link reconciliation failed: $INVALID_LINK" >&2
  false
}

REQUEST_ID="$(uuidgen | tr '[:upper:]' '[:lower:]')"
RUN_PATH="$RESOURCE_PATH/runs/$REQUEST_ID"
RUN_PAYLOAD="$(
  jq -n \
    --arg run "$RUN_PATH" \
    --arg request_id "$REQUEST_ID" \
    --arg resource "$RESOURCE_PATH" '{
      path: $run,
      metadata: {
        manifest: "/builtin/run",
        name: $request_id
      },
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
  jq -e '.status.metadata.state == "queued"' >/dev/null

RUN=""
for _ in $(seq 1 200); do
  RUN="$(
    curl --fail --silent --get \
      -H "Authorization: Bearer $ADMIN_TOKEN" \
      --data-urlencode "path=$RUN_PATH" \
      "$API/resources/by-path"
  )"
  if [[ "$(echo "$RUN" | jq -r '.status.metadata.state')" == "succeeded" ]]; then
    break
  fi
  sleep 0.05
done
echo "$RUN" | jq -e '
  .status.metadata.state == "succeeded"
  and .spec == .status.spec
  and .spec.output.echo.message == "hello from e2e"
' >/dev/null

# Updating a package replaces its managed definitions and restarts an already
# running Driver against the new content-addressed package directory.
DRIVER_GENERATION_BEFORE_UPDATE="$(
  curl --fail --silent --get \
    -H "Authorization: Bearer $ADMIN_TOKEN" \
    --data-urlencode "path=$DRIVER_PATH" \
    "$API/resources/by-path" |
    jq -r '.metadata["[kas]"].generation'
)"
PACKAGE_V2_ROOT="$E2E_DIR/package-v2"
mkdir -p "$PACKAGE_V2_ROOT"
cp -R "$PACKAGE_ROOT/." "$PACKAGE_V2_ROOT/"
jq '
  .version = 2
  | .description = "Updated real-process end-to-end test Manifest"
' "$PACKAGE_V2_ROOT/manifest.json" >"$PACKAGE_V2_ROOT/manifest.json.next"
mv "$PACKAGE_V2_ROOT/manifest.json.next" "$PACKAGE_V2_ROOT/manifest.json"
COPYFILE_DISABLE=1 tar -C "$PACKAGE_V2_ROOT" -cf "$E2E_DIR/echo-v2.kas" manifest.json resources driver

PACKAGE_V2="$(
  curl --fail --silent --show-error \
    -H "Authorization: Bearer $ADMIN_TOKEN" \
    -H "Content-Type: application/vnd.kas.manifest+tar" \
    --data-binary "@$E2E_DIR/echo-v2.kas" \
    "$API/packages"
)"
PACKAGE_V2_PATH="$(echo "$PACKAGE_V2" | jq -r '.path')"
echo "$PACKAGE_V2" | jq -e \
  --arg previous "$PACKAGE_PATH" '
    (.path | startswith("/packages/sha256/"))
    and .path != $previous
    and .metadata.manifest == "/builtin/package"
    and .spec.manifest == "/manifests/echo"
    and .spec.manifest_version == 2
    and .spec == .status.spec
  ' >/dev/null

UPDATED_DRIVER=""
for _ in $(seq 1 300); do
  UPDATED_DRIVER="$(
    curl --fail --silent --get \
      -H "Authorization: Bearer $ADMIN_TOKEN" \
      --data-urlencode "path=$DRIVER_PATH" \
      "$API/resources/by-path"
  )"
  if [[ "$(echo "$UPDATED_DRIVER" | jq -r '.status.metadata.state')" == "running" ]] &&
     (( "$(echo "$UPDATED_DRIVER" | jq -r '.metadata["[kas]"].generation')" > DRIVER_GENERATION_BEFORE_UPDATE )); then
    break
  fi
  sleep 0.05
done
echo "$UPDATED_DRIVER" | jq -e \
  --argjson previous_generation "$DRIVER_GENERATION_BEFORE_UPDATE" '
    .metadata.state == "running"
    and .status.metadata.state == "running"
    and .status.spec == .spec
    and .metadata["[kas]"].generation > $previous_generation
  ' >/dev/null || {
  echo "Updated Driver did not restart: $UPDATED_DRIVER" >&2
  false
}

curl --fail --silent --get \
  -H "Authorization: Bearer $ADMIN_TOKEN" \
  --data-urlencode "path=/manifests/echo" \
  "$API/resources/by-path" |
  jq -e '
    .spec.version == 2
    and .spec.description == "Updated real-process end-to-end test Manifest"
  ' >/dev/null

UPDATED_RESOURCE=""
for _ in $(seq 1 300); do
  UPDATED_RESOURCE="$(
    curl --fail --silent --get \
      -H "Authorization: Bearer $ADMIN_TOKEN" \
      --data-urlencode "path=$RESOURCE_PATH" \
      "$API/resources/by-path"
  )"
  if [[ "$(echo "$UPDATED_RESOURCE" | jq -r '.metadata["[kas]"].package')" == "$PACKAGE_V2_PATH" ]] &&
     [[ "$(echo "$UPDATED_RESOURCE" | jq -r '.status.metadata["[kas]"].package')" == "$PACKAGE_V2_PATH" ]]; then
    break
  fi
  sleep 0.05
done
echo "$UPDATED_RESOURCE" | jq -e \
  --arg package "$PACKAGE_V2_PATH" '
    .metadata["[kas]"].package == $package
    and .status.metadata["[kas]"].package == $package
  ' >/dev/null || {
  echo "Business Resource did not converge to updated Package: $UPDATED_RESOURCE" >&2
  false
}
OLD_PACKAGE_STATUS=""
for _ in $(seq 1 200); do
  OLD_PACKAGE_STATUS="$(
    curl --silent --output "$E2E_DIR/old-package.json" --write-out "%{http_code}" \
      --get \
      -H "Authorization: Bearer $ADMIN_TOKEN" \
      --data-urlencode "path=$PACKAGE_PATH" \
      "$API/resources/by-path"
  )"
  if [[ "$OLD_PACKAGE_STATUS" == "404" ]]; then
    break
  fi
  sleep 0.05
done
[[ "$OLD_PACKAGE_STATUS" == "404" ]]
curl --fail --silent --get \
  -H "Authorization: Bearer $ADMIN_TOKEN" \
  --data-urlencode "manifest=/builtin/link" \
  "$API/resources" |
  jq -e --arg package "$PACKAGE_V2_PATH" '
    [.[] | select(
      .spec.relation == "/builtin/relations/package-manifest"
      and .spec.target == "/manifests/echo"
    )] as $links
    | ($links | length) == 1
    and $links[0].spec.source == $package
  ' >/dev/null

UPDATED_REQUEST_ID="$(uuidgen | tr '[:upper:]' '[:lower:]')"
UPDATED_RUN_PATH="$RESOURCE_PATH/runs/$UPDATED_REQUEST_ID"
curl --fail --silent \
  -H "Authorization: Bearer $ADMIN_TOKEN" \
  -H "Content-Type: application/json" \
  -d "$(
    jq -n \
      --arg run "$UPDATED_RUN_PATH" \
      --arg request_id "$UPDATED_REQUEST_ID" \
      --arg resource "$RESOURCE_PATH" '{
        path: $run,
        metadata: {
          manifest: "/builtin/run",
          name: $request_id
        },
        spec: {
          request_id: $request_id,
          resource: $resource,
          action: "/manifests/echo/actions/echo",
          input: {message: "hello after package update"}
        }
      }'
  )" \
  "$API/resources" >/dev/null
UPDATED_RUN=""
for _ in $(seq 1 200); do
  UPDATED_RUN="$(
    curl --fail --silent --get \
      -H "Authorization: Bearer $ADMIN_TOKEN" \
      --data-urlencode "path=$UPDATED_RUN_PATH" \
      "$API/resources/by-path"
  )"
  if [[ "$(echo "$UPDATED_RUN" | jq -r '.status.metadata.state')" == "succeeded" ]]; then
    break
  fi
  sleep 0.05
done
echo "$UPDATED_RUN" | jq -e '
  .status.metadata.state == "succeeded"
  and .spec.output.echo.message == "hello after package update"
' >/dev/null || {
  echo "Updated Driver did not process a Run: $UPDATED_RUN" >&2
  false
}

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
    jq -r '.metadata["[kas]"].revision'
)"
curl --fail --silent --request DELETE \
  -H "Authorization: Bearer $ADMIN_TOKEN" \
  --get \
  --data-urlencode "path=$RESOURCE_PATH" \
  --data-urlencode "expected_revision=$RESOURCE_REVISION" \
  "$API/resources/by-path" |
  jq -e '.metadata.state == "deleted"' >/dev/null

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

for LINK_TO_DELETE in "$LINK_PATH" "$INVALID_LINK_PATH"; do
  LINK_STATUS=""
  for _ in $(seq 1 200); do
    LINK_STATUS="$(
      curl --silent --output "$E2E_DIR/deleted-link.json" --write-out "%{http_code}" \
        --get \
        -H "Authorization: Bearer $ADMIN_TOKEN" \
        --data-urlencode "path=$LINK_TO_DELETE" \
        "$API/resources/by-path"
    )"
    if [[ "$LINK_STATUS" == "404" ]]; then
      break
    fi
    sleep 0.05
  done
  [[ "$LINK_STATUS" == "404" ]] || {
    echo "Link cleanup failed for $LINK_TO_DELETE" >&2
    cat "$E2E_DIR/deleted-link.json" >&2
    false
  }
done

curl --fail --silent \
  -H "Authorization: Bearer $ADMIN_TOKEN" \
  -H "Content-Type: application/json" \
  -d "$RESOURCE_PAYLOAD" \
  "$API/resources" |
  jq -e '
    .path == "/resources/e2e/echo"
    and .metadata["[kas]"].revision == 0
    and .status.metadata.state == "pending"
  ' >/dev/null

curl --fail --silent \
  -H "Authorization: Bearer $ADMIN_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"path":"/users/e2e-viewer","metadata":{"manifest":"/builtin/user","name":"e2e-viewer"},"spec":{"disabled":false}}' \
  "$API/resources" >/dev/null
curl --fail --silent \
  -H "Authorization: Bearer $ADMIN_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"path":"/roles/e2e-viewer","metadata":{"manifest":"/builtin/role","name":"e2e-viewer"},"spec":{"rules":[{"manifests":["/manifests/echo"],"verbs":["get","download"],"paths":["/resources/e2e/**"]}]}}' \
  "$API/resources" >/dev/null
curl --fail --silent \
  -H "Authorization: Bearer $ADMIN_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"path":"/links/e2e-viewer-role","metadata":{"manifest":"/builtin/link","name":"e2e-viewer-role"},"spec":{"relation":"/builtin/relations/role-binding","source":"/users/e2e-viewer","target":"/roles/e2e-viewer","metadata":{}}}' \
  "$API/resources" >/dev/null
VIEWER_CREDENTIAL="$(
  curl --fail --silent \
    -H "Authorization: Bearer $ADMIN_TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"subject":"/users/e2e-viewer"}' \
    "$API/credentials/issue"
)"
VIEWER_TOKEN="$(jq -r '.token' <<<"$VIEWER_CREDENTIAL")"
VIEWER_CREDENTIAL_PATH="$(jq -r '.resource_path' <<<"$VIEWER_CREDENTIAL")"

curl --fail --silent \
  -H "Authorization: Bearer $VIEWER_TOKEN" \
  "$API/auth" |
  jq -e --arg credential "$VIEWER_CREDENTIAL_PATH" '
    .credential_path == $credential
    and .subject == {
      path: "/users/e2e-viewer",
      manifest: "/builtin/user"
    }
    and .rules == [{
      manifests: ["/manifests/echo"],
      verbs: ["get", "download"],
      paths: ["/resources/e2e/**"]
    }]
    and .driver_path == null
    and .driver_generation == null
  ' >/dev/null

curl --fail --silent \
  -H "Authorization: Bearer $VIEWER_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"manifest":"/manifests/echo","verb":"get","path":"/resources/e2e/echo"}' \
  "$API/auth/check" |
  jq -e --arg credential "$VIEWER_CREDENTIAL_PATH" '
    .allowed == true
    and .credential_path == $credential
    and .subject.path == "/users/e2e-viewer"
  ' >/dev/null

curl --fail --silent \
  -H "Authorization: Bearer $VIEWER_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"manifest":"/manifests/echo","verb":"download","path":"/resources/e2e/echo"}' \
  "$API/auth/check" |
  jq -e '.allowed == true' >/dev/null

curl --fail --silent \
  -H "Authorization: Bearer $VIEWER_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"manifest":"/manifests/echo","verb":"update","path":"/resources/e2e/echo"}' \
  "$API/auth/check" |
  jq -e '
    .allowed == false
    and .subject.path == "/users/e2e-viewer"
  ' >/dev/null

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
  -d "{\"path\":\"$DRIVER_PATH\",\"state\":\"stopped\"}" \
  "$API/drivers/control" >/dev/null

for _ in $(seq 1 100); do
  DRIVER="$(
    curl --fail --silent --get \
      -H "Authorization: Bearer $ADMIN_TOKEN" \
      --data-urlencode "path=$DRIVER_PATH" \
      "$API/resources/by-path"
  )"
  if [[ "$(echo "$DRIVER" | jq -r '.status.metadata.state')" == "stopped" ]]; then
    break
  fi
  sleep 0.05
done
echo "$DRIVER" | jq -e '
  .metadata.state == "stopped"
  and .status.metadata.state == "stopped"
  and .status.spec == .spec
' >/dev/null

TABLES="$(sqlite3 "$KAS_DATABASE" \
  "SELECT group_concat(name, ',') FROM (SELECT name FROM sqlite_schema WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name)")"
[[ "$TABLES" == "events,resources" ]]

echo "KAS end-to-end test passed"
