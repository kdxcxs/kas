-- The path model intentionally replaces the unpublished UUID object model.
-- Existing data is discarded: there is no compatibility contract for the
-- pre-path schema.
DROP TABLE driver_deliveries;
DROP TABLE events;
DROP TABLE links;
DROP TABLE runs;
DROP TABLE resources;
DROP TABLE credentials;
DROP TABLE role_binding_subjects;
DROP TABLE role_bindings;
DROP TABLE roles;
DROP TABLE service_accounts;
DROP TABLE users;
DROP TABLE drivers;
DROP TABLE manifests;

CREATE TABLE manifests (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    version INTEGER NOT NULL CHECK(version>0),
    description TEXT NOT NULL,
    resource_schema_json TEXT NOT NULL CHECK(json_valid(resource_schema_json)),
    actions_json TEXT NOT NULL CHECK(json_valid(actions_json)),
    driver_name TEXT,
    created_at TEXT NOT NULL,
    UNIQUE(name,version)
) STRICT;

CREATE TABLE drivers (
    id TEXT PRIMARY KEY,
    manifest_path TEXT NOT NULL UNIQUE REFERENCES manifests(id) ON DELETE RESTRICT,
    name TEXT NOT NULL,
    state TEXT NOT NULL CHECK(state IN ('stopped','starting','ready','stopping','failed')),
    generation INTEGER NOT NULL CHECK(generation>=0),
    process_id INTEGER,
    metadata_json TEXT NOT NULL CHECK(json_valid(metadata_json)),
    started_at TEXT,
    heartbeat_at TEXT,
    stopped_at TEXT,
    error TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
) STRICT;

CREATE TABLE resources (
    id TEXT PRIMARY KEY,
    manifest_path TEXT NOT NULL REFERENCES manifests(id) ON DELETE RESTRICT,
    name TEXT NOT NULL,
    spec_json TEXT NOT NULL CHECK(json_valid(spec_json)),
    status_json TEXT NOT NULL CHECK(json_valid(status_json)),
    revision INTEGER NOT NULL CHECK(revision>=0),
    observed_revision INTEGER NOT NULL DEFAULT -1,
    claimed_revision INTEGER,
    claim_driver_generation INTEGER,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
) STRICT;

CREATE INDEX resources_manifest_path
ON resources(manifest_path,created_at,id);

CREATE TABLE runs (
    id TEXT PRIMARY KEY,
    request_id TEXT NOT NULL UNIQUE,
    resource_path TEXT NOT NULL REFERENCES resources(id) ON DELETE RESTRICT,
    driver_path TEXT NOT NULL REFERENCES drivers(id) ON DELETE RESTRICT,
    driver_generation INTEGER,
    action TEXT NOT NULL,
    input_json TEXT NOT NULL CHECK(json_valid(input_json)),
    status TEXT NOT NULL CHECK(status IN ('queued','running','succeeded','failed','cancelled')),
    output_json TEXT CHECK(output_json IS NULL OR json_valid(output_json)),
    error TEXT,
    created_at TEXT NOT NULL,
    started_at TEXT,
    finished_at TEXT
) STRICT;

CREATE INDEX queued_runs
ON runs(driver_path,status,created_at,id);

CREATE TABLE links (
    id TEXT PRIMARY KEY,
    source_kind TEXT NOT NULL,
    source_path TEXT NOT NULL,
    relation TEXT NOT NULL,
    target_kind TEXT NOT NULL,
    target_path TEXT NOT NULL,
    metadata_json TEXT NOT NULL CHECK(json_valid(metadata_json)),
    created_at TEXT NOT NULL,
    UNIQUE(source_kind,source_path,relation,target_kind,target_path)
) STRICT;

CREATE TABLE events (
    sequence INTEGER PRIMARY KEY AUTOINCREMENT,
    event_type TEXT NOT NULL CHECK(event_type IN ('created','updated','deleted')),
    object_kind TEXT NOT NULL CHECK(object_kind IN ('resource','link','run')),
    object_path TEXT NOT NULL,
    manifest_path TEXT REFERENCES manifests(id) ON DELETE RESTRICT,
    revision INTEGER CHECK(revision IS NULL OR revision>=0),
    value_json TEXT NOT NULL CHECK(json_valid(value_json)),
    created_at TEXT NOT NULL
) STRICT;

CREATE INDEX events_object_sequence
ON events(object_kind,object_path,sequence);

CREATE INDEX events_manifest_sequence
ON events(manifest_path,sequence);

CREATE TABLE users (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    disabled INTEGER NOT NULL DEFAULT 0 CHECK(disabled IN (0,1)),
    created_at TEXT NOT NULL
) STRICT;

CREATE TABLE service_accounts (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    driver_path TEXT UNIQUE REFERENCES drivers(id) ON DELETE RESTRICT,
    managed_by TEXT NOT NULL CHECK(managed_by IN ('system','user')),
    created_at TEXT NOT NULL
) STRICT;

CREATE TABLE roles (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    description TEXT NOT NULL,
    rules_json TEXT NOT NULL CHECK(json_valid(rules_json)),
    managed_by TEXT NOT NULL CHECK(managed_by IN ('system','user')),
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
) STRICT;

CREATE TABLE role_bindings (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    role_path TEXT NOT NULL REFERENCES roles(id) ON DELETE RESTRICT,
    managed_by TEXT NOT NULL CHECK(managed_by IN ('system','user')),
    created_at TEXT NOT NULL
) STRICT;

CREATE TABLE role_binding_subjects (
    role_binding_path TEXT NOT NULL REFERENCES role_bindings(id) ON DELETE CASCADE,
    subject_kind TEXT NOT NULL CHECK(subject_kind IN ('user','service_account')),
    subject_path TEXT NOT NULL,
    PRIMARY KEY(role_binding_path,subject_kind,subject_path)
) STRICT;

CREATE INDEX role_binding_subject_lookup
ON role_binding_subjects(subject_kind,subject_path,role_binding_path);

CREATE TABLE credentials (
    id TEXT PRIMARY KEY,
    subject_kind TEXT NOT NULL CHECK(subject_kind IN ('user','service_account')),
    subject_path TEXT NOT NULL,
    token_hash TEXT NOT NULL UNIQUE,
    driver_generation INTEGER,
    expires_at TEXT,
    revoked_at TEXT,
    created_at TEXT NOT NULL
) STRICT;

CREATE TABLE driver_deliveries (
    id TEXT PRIMARY KEY,
    driver_path TEXT NOT NULL REFERENCES drivers(id) ON DELETE RESTRICT,
    generation INTEGER NOT NULL CHECK(generation>=0),
    work_json TEXT NOT NULL CHECK(json_valid(work_json)),
    status TEXT NOT NULL CHECK(status IN ('pending','acked','completed')),
    created_at TEXT NOT NULL,
    acked_at TEXT,
    completed_at TEXT
) STRICT;

CREATE INDEX driver_deliveries_replay
ON driver_deliveries(driver_path,generation,status,created_at,id);
