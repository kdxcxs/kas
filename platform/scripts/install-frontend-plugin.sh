#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 8 ]]; then
  echo "usage: $0 ZIP_FILE PLUGIN_PATH SLUG ENTRYPOINT LABEL ICON ORDER ROUTE" >&2
  echo "requires KAS_API_URL, KAS_FILE_API_URL and KAS_TOKEN" >&2
  exit 2
fi

for command in curl jq; do
  command -v "$command" >/dev/null || {
    echo "missing required command: $command" >&2
    exit 1
  }
done

ZIP_FILE="$1"
PLUGIN_PATH="$2"
SLUG="$3"
ENTRYPOINT="$4"
LABEL="$5"
ICON="$6"
ORDER="$7"
ROUTE="$8"
API="${KAS_API_URL:?KAS_API_URL is required}"
FILE_API="${KAS_FILE_API_URL:?KAS_FILE_API_URL is required}"
TOKEN="${KAS_TOKEN:?KAS_TOKEN is required}"

if [[ ! -f "$ZIP_FILE" ]]; then
  echo "plugin ZIP file does not exist: $ZIP_FILE" >&2
  exit 1
fi

FILE_PATH="/files${PLUGIN_PATH}/bundle"
FILE="$(
  curl --fail-with-body --silent --show-error \
    -H "Authorization: Bearer $TOKEN" \
    -F "content=@$ZIP_FILE;type=application/zip" \
    "$FILE_API/files?$(jq -rn --arg value "$FILE_PATH" '$value|@uri' | sed 's/^/path=/')"
)"

curl --fail-with-body --silent --show-error \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d "$(
    jq -n \
      --arg path "$PLUGIN_PATH" \
      --arg name "$LABEL" \
      --arg id "$SLUG" \
      --arg slug "$SLUG" \
      --arg entrypoint "$ENTRYPOINT" \
      --arg label "$LABEL" \
      --arg icon "$ICON" \
      --argjson order "$ORDER" \
      --arg route "$ROUTE" '{
        metadata: {
          path: $path,
          manifest: "/manifests/frontend-plugin",
          name: $name
        },
        spec: {
          api_version: 1,
          slug: $slug,
          entrypoint: $entrypoint,
          contributes: {
            sidebar: [{
              id: $id,
              label: $label,
              description: "Installed frontend plugin",
              icon: $icon,
              section: "workspace",
              order: $order,
              route: $route
            }]
          }
        }
      }'
  )" \
  "$API/resources" >/dev/null

curl --fail-with-body --silent --show-error \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d "$(
    jq -n \
      --arg path "$PLUGIN_PATH/links/bundle" \
      --arg source "$PLUGIN_PATH" \
      --arg target "$(jq -r '.metadata.path' <<<"$FILE")" '{
        metadata: {
          path: $path,
          manifest: "/builtin/link",
          name: "bundle"
        },
        spec: {
          relation: "/manifests/frontend-plugin/relations/bundle",
          source: $source,
          target: $target,
          metadata: {}
        }
      }'
  )" \
  "$API/resources" >/dev/null

jq -n \
  --arg plugin "$PLUGIN_PATH" \
  --arg file "$(jq -r '.metadata.path' <<<"$FILE")" \
  '{plugin: $plugin, file: $file}'
