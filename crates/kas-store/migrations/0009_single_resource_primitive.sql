-- KAS v2 has one canonical persistent primitive: Resource.
--
-- This is intentionally destructive. The pre-v2 schema was never published
-- and encoded each platform concept as a different canonical table. Runtime
-- and query-specific tables below are projections keyed by Resource path.

DROP TABLE driver_deliveries;
DROP TABLE reconcile_queue;
DROP TABLE role_binding_subjects;
DROP TABLE role_binding_role_index;
DROP TABLE run_relation_index;
DROP TABLE resource_manifest_index;
DROP TABLE driver_service_account_index;
DROP TABLE credentials;
DROP TABLE service_accounts;
DROP TABLE links;
DROP TABLE resources;
DROP TABLE driver_manifest_index;
DROP TABLE drivers;
DROP TABLE relations;
DROP TABLE actions;
DROP TABLE role_bindings;
DROP TABLE roles;
DROP TABLE users;
DROP TABLE runs;
DROP TABLE manifests;
DROP TABLE events;
DROP TABLE packages;

CREATE TABLE resources (
    path TEXT PRIMARY KEY,
    manifest_path TEXT NOT NULL,
    name TEXT NOT NULL,
    spec_json TEXT NOT NULL CHECK(json_valid(spec_json)),
    status_json TEXT NOT NULL CHECK(json_valid(status_json)),
    revision INTEGER NOT NULL DEFAULT 0 CHECK(revision >= 0),
    protected INTEGER NOT NULL DEFAULT 0 CHECK(protected IN (0,1)),
    managed_by TEXT NOT NULL DEFAULT 'user',
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    FOREIGN KEY(manifest_path) REFERENCES resources(path)
        ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED
) STRICT;

CREATE INDEX resources_by_manifest
ON resources(manifest_path,created_at,path);

-- A Link is a Resource whose manifest is /builtin/link. This table is
-- only the searchable projection of its spec.
CREATE TABLE link_index (
    link_path TEXT PRIMARY KEY REFERENCES resources(path) ON DELETE CASCADE,
    relation_path TEXT NOT NULL REFERENCES resources(path) ON DELETE RESTRICT,
    source_path TEXT REFERENCES resources(path) ON DELETE RESTRICT,
    target_path TEXT REFERENCES resources(path) ON DELETE RESTRICT,
    CHECK(source_path IS NOT NULL OR target_path IS NOT NULL),
    UNIQUE(source_path,relation_path,target_path)
) STRICT;

CREATE INDEX links_by_source
ON link_index(source_path,relation_path,link_path)
WHERE source_path IS NOT NULL;

CREATE INDEX links_by_target
ON link_index(target_path,relation_path,link_path)
WHERE target_path IS NOT NULL;

-- Definition and runtime projections. Their authoritative values remain in
-- the corresponding Resource spec/status documents.
CREATE TABLE manifest_index (
    manifest_path TEXT PRIMARY KEY REFERENCES resources(path) ON DELETE CASCADE,
    version INTEGER NOT NULL CHECK(version > 0)
) STRICT;

CREATE TABLE relation_index (
    relation_path TEXT PRIMARY KEY REFERENCES resources(path) ON DELETE CASCADE,
    owner_manifest_path TEXT NOT NULL REFERENCES resources(path) ON DELETE CASCADE,
    role TEXT,
    relation_type TEXT NOT NULL
        CHECK(relation_type IN ('one_to_one','one_to_many','many_to_one','many_to_many')),
    ensure INTEGER NOT NULL DEFAULT 0 CHECK(ensure IN (0,1)),
    on_source_delete TEXT NOT NULL DEFAULT 'unlink'
        CHECK(on_source_delete IN ('unlink','cascade'))
) STRICT;

CREATE UNIQUE INDEX relation_role
ON relation_index(role)
WHERE role IS NOT NULL;

CREATE TABLE action_index (
    action_path TEXT PRIMARY KEY REFERENCES resources(path) ON DELETE CASCADE,
    owner_manifest_path TEXT NOT NULL REFERENCES resources(path) ON DELETE CASCADE
) STRICT;

CREATE TABLE driver_runtime (
    driver_path TEXT PRIMARY KEY REFERENCES resources(path) ON DELETE CASCADE,
    owner_manifest_path TEXT NOT NULL UNIQUE REFERENCES resources(path) ON DELETE CASCADE,
    generation INTEGER NOT NULL DEFAULT 0 CHECK(generation >= 0),
    process_id INTEGER,
    started_at TEXT,
    heartbeat_at TEXT,
    stopped_at TEXT,
    error TEXT
) STRICT;

CREATE TABLE run_runtime (
    run_path TEXT PRIMARY KEY REFERENCES resources(path) ON DELETE CASCADE,
    request_id TEXT NOT NULL UNIQUE,
    resource_path TEXT NOT NULL REFERENCES resources(path) ON DELETE RESTRICT,
    action_path TEXT NOT NULL REFERENCES resources(path) ON DELETE RESTRICT,
    driver_path TEXT NOT NULL REFERENCES resources(path) ON DELETE RESTRICT,
    driver_generation INTEGER,
    started_at TEXT,
    finished_at TEXT
) STRICT;

CREATE INDEX runs_by_driver
ON run_runtime(driver_path,run_path);

CREATE TABLE driver_service_accounts (
    driver_path TEXT PRIMARY KEY REFERENCES resources(path) ON DELETE CASCADE,
    service_account_path TEXT NOT NULL UNIQUE REFERENCES resources(path) ON DELETE RESTRICT,
    link_path TEXT NOT NULL UNIQUE REFERENCES resources(path) ON DELETE RESTRICT
) STRICT;

CREATE TABLE role_binding_roles (
    role_binding_path TEXT PRIMARY KEY REFERENCES resources(path) ON DELETE CASCADE,
    role_path TEXT NOT NULL REFERENCES resources(path) ON DELETE RESTRICT,
    link_path TEXT NOT NULL UNIQUE REFERENCES resources(path) ON DELETE RESTRICT
) STRICT;

CREATE TABLE role_binding_subjects (
    role_binding_path TEXT NOT NULL REFERENCES resources(path) ON DELETE CASCADE,
    subject_path TEXT NOT NULL REFERENCES resources(path) ON DELETE CASCADE,
    link_path TEXT NOT NULL UNIQUE REFERENCES resources(path) ON DELETE RESTRICT,
    PRIMARY KEY(role_binding_path,subject_path)
) STRICT;

CREATE INDEX role_binding_subject_lookup
ON role_binding_subjects(subject_path,role_binding_path);

-- Credential metadata is a normal Resource. Only token material remains
-- private and cannot be returned through Resource reads.
CREATE TABLE credential_material (
    credential_path TEXT PRIMARY KEY REFERENCES resources(path) ON DELETE CASCADE,
    token_hash TEXT NOT NULL UNIQUE,
    driver_generation INTEGER
) STRICT;

CREATE TABLE events (
    sequence INTEGER PRIMARY KEY AUTOINCREMENT,
    event_type TEXT NOT NULL CHECK(event_type IN ('created','updated','deleted')),
    resource_path TEXT NOT NULL,
    revision INTEGER,
    value_json TEXT NOT NULL CHECK(json_valid(value_json)),
    created_at TEXT NOT NULL
) STRICT;

CREATE INDEX events_by_resource
ON events(resource_path,sequence);

CREATE TABLE reconcile_queue (
    resource_path TEXT PRIMARY KEY REFERENCES resources(path) ON DELETE CASCADE,
    driver_path TEXT NOT NULL REFERENCES resources(path) ON DELETE CASCADE,
    reason TEXT NOT NULL,
    available_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
) STRICT;

CREATE INDEX reconcile_queue_ready
ON reconcile_queue(driver_path,available_at,updated_at,resource_path);

CREATE TABLE driver_deliveries (
    id TEXT PRIMARY KEY,
    driver_path TEXT NOT NULL REFERENCES resources(path) ON DELETE RESTRICT,
    generation INTEGER NOT NULL CHECK(generation >= 0),
    work_json TEXT NOT NULL CHECK(json_valid(work_json)),
    status TEXT NOT NULL CHECK(status IN ('pending','acked','completed')),
    created_at TEXT NOT NULL,
    acked_at TEXT,
    completed_at TEXT
) STRICT;

CREATE INDEX driver_deliveries_replay
ON driver_deliveries(driver_path,generation,status,created_at,id);
