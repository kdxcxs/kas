#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CONTAINER="kas-postgres-e2e-$(uuidgen | tr '[:upper:]' '[:lower:]')"
PASSWORD="kas-e2e"

cleanup() {
  local status=$?
  if (( status != 0 )); then
    echo "PostgreSQL container output:" >&2
    docker logs "$CONTAINER" 2>&1 | tail -n 400 >&2 || true
  fi
  docker rm -f "$CONTAINER" >/dev/null 2>&1 || true
  return "$status"
}
trap cleanup EXIT

docker run --detach --rm \
  --name "$CONTAINER" \
  -e POSTGRES_PASSWORD="$PASSWORD" \
  -e POSTGRES_USER=kas \
  -e POSTGRES_DB=kas \
  -p 127.0.0.1::5432 \
  postgres:17-alpine \
  -c log_parameter_max_length_on_error=-1 >/dev/null

for _ in $(seq 1 120); do
  if docker exec "$CONTAINER" psql -U kas -d kas -Atqc "SELECT 1" 2>/dev/null |
    grep -qx '1'; then
    break
  fi
  sleep 0.25
done
docker exec "$CONTAINER" psql -U kas -d kas -Atqc "SELECT 1" |
  grep -qx '1'

PORT="$(
  docker inspect \
    --format '{{(index (index .NetworkSettings.Ports "5432/tcp") 0).HostPort}}' \
    "$CONTAINER"
)"
DATABASE="postgresql://kas:$PASSWORD@127.0.0.1:$PORT/kas"

KAS_E2E_DATABASE="$DATABASE" \
KAS_E2E_SKIP_STORAGE_ASSERT=true \
  "$ROOT/tests/e2e.sh" "$@"

docker exec "$CONTAINER" psql -U kas -d kas -Atqc "
  SELECT string_agg(table_name, ',' ORDER BY table_name)
  FROM information_schema.tables
  WHERE table_schema='public';
" | grep -qx 'events,kas_schema,resources'

docker exec "$CONTAINER" psql -U kas -d kas -Atqc "
  SELECT string_agg(column_name || ':' || data_type, ',' ORDER BY ordinal_position)
  FROM information_schema.columns
  WHERE table_schema='public' AND table_name='resources';
" | grep -qx 'path:text,metadata:jsonb,spec:jsonb,status:jsonb'

docker exec "$CONTAINER" psql -U kas -d kas -Atqc "
  SELECT version FROM kas_schema;
" | grep -qx '17'

docker exec "$CONTAINER" createdb -U kas kas_upgrade
docker exec "$CONTAINER" psql -U kas -d kas_upgrade -v ON_ERROR_STOP=1 -qc "
  CREATE TABLE kas_schema(version INTEGER NOT NULL);
  INSERT INTO kas_schema VALUES (16);
  CREATE TABLE resources(
    path TEXT PRIMARY KEY,
    metadata TEXT NOT NULL,
    spec TEXT NOT NULL,
    status TEXT NOT NULL
  );
  CREATE TABLE events(
    sequence BIGSERIAL PRIMARY KEY,
    event_type TEXT NOT NULL,
    resource_path TEXT NOT NULL,
    revision BIGINT NOT NULL,
    value_json TEXT NOT NULL,
    created_at TEXT NOT NULL
  );
  INSERT INTO resources VALUES (
    '/upgrade/test',
    '{\"manifest\":\"/upgrade\",\"name\":\"test\",\"state\":\"available\",\"[kas]\":{\"created_at\":\"2026-01-01T00:00:00Z\",\"package\":\"\"}}',
    '{}',
    '{\"metadata\":{\"state\":\"available\",\"[kas]\":{\"package\":\"\"}},\"spec\":{}}'
  );
  INSERT INTO events(event_type,resource_path,revision,value_json,created_at)
  VALUES ('created','/upgrade/test',0,'{}','2026-01-01T00:00:00Z');
"

KAS_DATABASE="postgresql://kas:$PASSWORD@127.0.0.1:$PORT/kas_upgrade" \
  "$ROOT/target/debug/kas-migrate" >/dev/null

docker exec "$CONTAINER" psql -U kas -d kas_upgrade -Atqc "
  SELECT version || ':' ||
         (SELECT data_type FROM information_schema.columns
          WHERE table_schema='public' AND table_name='resources' AND column_name='metadata') || ':' ||
         (SELECT data_type FROM information_schema.columns
          WHERE table_schema='public' AND table_name='events' AND column_name='created_at')
  FROM kas_schema;
" | grep -qx '17:jsonb:timestamp with time zone'

echo "KAS PostgreSQL end-to-end test passed"
