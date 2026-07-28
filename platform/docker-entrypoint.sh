#!/usr/bin/env bash
set -euo pipefail

PACKAGES_DIR="${KAS_PACKAGES_DIR:-/opt/kas/packages}"
PLUGINS_DIR="${KAS_PLUGINS_DIR:-/opt/kas/plugins}"
INSTALL_PLUGIN="${KAS_INSTALL_PLUGIN:-/opt/kas/scripts/install-frontend-plugin.sh}"
INSTALL_MARKER="$KAS_DATA_DIR/.platform-installed"
API="${KAS_API_URL:-http://127.0.0.1:3000}"
ADMIN_NAME="${KAS_ADMIN_NAME:-admin}"
API_PID=""

stop_api() {
  if [[ -n "$API_PID" ]] && kill -0 "$API_PID" 2>/dev/null; then
    kill "$API_PID" 2>/dev/null || true
    wait "$API_PID" 2>/dev/null || true
  fi
}
trap stop_api EXIT INT TERM

mkdir -p "$KAS_DATA_DIR" "$KAS_CODEX_HOME"
chmod 700 "$KAS_CODEX_HOME"

kas-migrate

ADMIN_TOKEN=""
if [[ ! -f "$INSTALL_MARKER" ]]; then
  ADMIN_TOKEN="$(kas-admin bootstrap "$ADMIN_NAME")"
fi

kas-api &
API_PID="$!"

for _ in $(seq 1 200); do
  if curl --fail --silent "$API/health" >/dev/null; then
    break
  fi
  if ! kill -0 "$API_PID" 2>/dev/null; then
    echo "kas-api exited during startup" >&2
    exit 1
  fi
  sleep 0.05
done
curl --fail --silent "$API/health" >/dev/null

if [[ ! -f "$INSTALL_MARKER" ]]; then
  install_package() {
    local package="$1" response_file http_code
    response_file="$(mktemp)"
    http_code="$(
      curl --silent --show-error \
        --output "$response_file" \
        --write-out '%{http_code}' \
        -H "Authorization: Bearer $ADMIN_TOKEN" \
        -H "Content-Type: application/vnd.kas.manifest+tar" \
        --data-binary "@$package" \
        "$API/packages"
    )"
    if [[ "$http_code" != "200" && "$http_code" != "201" && "$http_code" != "409" ]]; then
      cat "$response_file" >&2
      return 1
    fi
  }

  echo "Installing KAS Platform packages..."
  for package in \
    thread session file proxy frontend skill approval-result approval agent message telegram; do
    install_package "$PACKAGES_DIR/$package.kas"
  done

  create_proxy() {
    local path="$1" name="$2" prefix="$3" upstream="$4"
    if curl --fail --silent --get \
      -H "Authorization: Bearer $ADMIN_TOKEN" \
      --data-urlencode "path=$path" \
      "$API/resources/by-path" >/dev/null 2>&1; then
      return
    fi
    curl --fail-with-body --silent --show-error \
      -H "Authorization: Bearer $ADMIN_TOKEN" \
      -H "Content-Type: application/json" \
      -d "$(
        jq -cn \
          --arg path "$path" \
          --arg name "$name" \
          --arg prefix "$prefix" \
          --arg upstream "$upstream" '{
            path: $path,
            metadata: {
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
  create_proxy "/proxies/file" "File API" "/files-api" "http://127.0.0.1:3001"
  create_proxy "/proxies/skill" "Skill API" "/skills-api" "http://127.0.0.1:3002"
  create_proxy "/proxies/approval" "Approval API" "/approvals-api" "http://127.0.0.1:3003"

  wait_for_state() {
    local path="$1" state="$2" resource=""
    for _ in $(seq 1 400); do
      resource="$(
        curl --fail --silent --get \
          -H "Authorization: Bearer $ADMIN_TOKEN" \
          --data-urlencode "path=$path" \
          "$API/resources/by-path" 2>/dev/null || true
      )"
      if [[ "$(jq -r '.status.metadata.state // empty' <<<"$resource")" == "$state" ]]; then
        return
      fi
      sleep 0.05
    done
    echo "Resource did not reach $state: $path" >&2
    return 1
  }

  wait_for_state "/manifests/file/driver" running
  wait_for_state "/manifests/frontend-plugin/driver" running
  wait_for_state "/manifests/telegram/driver" running
  wait_for_state "/proxies/file" available
  wait_for_state "/proxies/skill" available
  wait_for_state "/proxies/approval" available
  curl --fail --silent "http://127.0.0.1:3001/health" >/dev/null

  install_plugin() {
    local archive="$1" path="$2" slug="$3" entrypoint="$4"
    local label="$5" icon="$6" order="$7" route="$8"
    if curl --fail --silent --get \
      -H "Authorization: Bearer $ADMIN_TOKEN" \
      --data-urlencode "path=$path" \
      "$API/resources/by-path" >/dev/null 2>&1; then
      return
    fi
    KAS_API_URL="$API" \
    KAS_FILE_API_URL="http://127.0.0.1:3001" \
    KAS_TOKEN="$ADMIN_TOKEN" \
      "$INSTALL_PLUGIN" \
        "$archive" "$path" "$slug" "$entrypoint" \
        "$label" "$icon" "$order" "$route" >/dev/null
  }

  install_plugin "$PLUGINS_DIR/workspace.zip" \
    "/frontend-plugins/threads" threads threads.html Threads "#" 10 /threads
  install_plugin "$PLUGINS_DIR/workspace.zip" \
    "/frontend-plugins/agents" agents agents.html Agents A 20 /agents
  install_plugin "$PLUGINS_DIR/workspace.zip" \
    "/frontend-plugins/skills" skills skills.html Skills "⌁" 30 /skills
  install_plugin "$PLUGINS_DIR/workspace.zip" \
    "/frontend-plugins/approvals" approvals approvals.html Approvals "✓" 40 /approvals
  install_plugin "$PLUGINS_DIR/workspace.zip" \
    "/frontend-plugins/telegram" telegram telegram.html Telegram "✈" 45 /telegram
  install_plugin "$PLUGINS_DIR/registry.zip" \
    "/frontend-plugins/registry" registry index.html Objects "◇" 50 /objects

  for plugin in threads agents skills approvals telegram registry; do
    wait_for_state "/frontend-plugins/$plugin" available
  done

  touch "$INSTALL_MARKER"
  echo
  echo "KAS Platform initialized."
  echo "Admin user:  /users/$ADMIN_NAME"
  echo "Admin token: $ADMIN_TOKEN"
  echo "Save this token now; it will not be printed again."
  echo
fi

wait "$API_PID"
