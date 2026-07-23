-- Manifest packages, first-class Actions/Relations, and relation-backed object
-- ownership replace the unpublished v6 model. Existing data is intentionally
-- discarded; this migration establishes the complete schema for a fresh KAS.
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

CREATE TABLE packages (
    digest TEXT PRIMARY KEY CHECK(
        length(digest)=71
        AND substr(digest,1,7)='sha256:'
        AND substr(digest,8) NOT GLOB '*[^0-9a-f]*'
    ),
    size_bytes INTEGER NOT NULL CHECK(size_bytes>=0),
    installed_at TEXT NOT NULL
) STRICT;

CREATE TABLE manifests (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    version INTEGER NOT NULL CHECK(version>0),
    description TEXT NOT NULL,
    resource_schema_json TEXT NOT NULL CHECK(json_valid(resource_schema_json)),
    package_digest TEXT NOT NULL REFERENCES packages(digest) ON DELETE RESTRICT,
    created_at TEXT NOT NULL,
    UNIQUE(name,version)
) STRICT;

CREATE TABLE actions (
    id TEXT PRIMARY KEY,
    manifest_path TEXT NOT NULL REFERENCES manifests(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    description TEXT NOT NULL,
    input_schema_json TEXT NOT NULL CHECK(json_valid(input_schema_json)),
    output_schema_json TEXT NOT NULL CHECK(json_valid(output_schema_json)),
    created_at TEXT NOT NULL,
    UNIQUE(manifest_path,name)
) STRICT;

CREATE INDEX actions_manifest
ON actions(manifest_path,id);

CREATE TABLE relations (
    id TEXT PRIMARY KEY,
    manifest_path TEXT REFERENCES manifests(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    role TEXT UNIQUE CHECK(role IS NULL OR role IN (
        'manifest_member','resource_manifest','run_resource','run_action','run_driver',
        'driver_service_account','role_binding_role','role_binding_subject'
    )),
    inverse_name TEXT,
    sources_json TEXT NOT NULL CHECK(json_valid(sources_json)),
    targets_json TEXT NOT NULL CHECK(json_valid(targets_json)),
    cardinality_json TEXT NOT NULL CHECK(json_valid(cardinality_json)),
    metadata_schema_json TEXT NOT NULL CHECK(json_valid(metadata_schema_json)),
    protected INTEGER NOT NULL DEFAULT 0 CHECK(protected IN (0,1)),
    created_at TEXT NOT NULL,
    UNIQUE(manifest_path,name)
) STRICT;

CREATE INDEX relations_manifest
ON relations(manifest_path,id);

CREATE TABLE drivers (
    id TEXT PRIMARY KEY,
    package_digest TEXT NOT NULL REFERENCES packages(digest) ON DELETE RESTRICT,
    runtime TEXT NOT NULL CHECK(runtime='process'),
    entrypoint TEXT NOT NULL,
    args_json TEXT NOT NULL CHECK(json_valid(args_json)),
    restart_policy TEXT NOT NULL CHECK(restart_policy IN ('never','on_failure','always')),
    desired_state TEXT NOT NULL CHECK(desired_state IN ('stopped','running')),
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

-- Internal projection derived from the Driver's canonical nested path. Driver
-- ownership is not represented by a Link because it is intrinsic to the
-- Manifest package layout.
CREATE TABLE driver_manifest_index (
    driver_path TEXT PRIMARY KEY REFERENCES drivers(id) ON DELETE CASCADE,
    manifest_path TEXT NOT NULL UNIQUE REFERENCES manifests(id) ON DELETE CASCADE
) STRICT;

CREATE TABLE driver_service_account_index (
    driver_path TEXT PRIMARY KEY REFERENCES drivers(id) ON DELETE CASCADE,
    service_account_path TEXT NOT NULL UNIQUE REFERENCES service_accounts(id) ON DELETE RESTRICT,
    link_path TEXT NOT NULL UNIQUE REFERENCES links(id) ON DELETE RESTRICT
) STRICT;

CREATE TABLE resources (
    id TEXT PRIMARY KEY,
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

CREATE TABLE runs (
    id TEXT PRIMARY KEY,
    request_id TEXT NOT NULL UNIQUE,
    driver_generation INTEGER,
    input_json TEXT NOT NULL CHECK(json_valid(input_json)),
    status TEXT NOT NULL CHECK(status IN ('queued','running','succeeded','failed','cancelled')),
    output_json TEXT CHECK(output_json IS NULL OR json_valid(output_json)),
    error TEXT,
    created_at TEXT NOT NULL,
    started_at TEXT,
    finished_at TEXT
) STRICT;

CREATE TABLE users (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    disabled INTEGER NOT NULL DEFAULT 0 CHECK(disabled IN (0,1)),
    created_at TEXT NOT NULL
) STRICT;

CREATE TABLE service_accounts (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    managed_by TEXT NOT NULL CHECK(managed_by='user' OR managed_by LIKE 'package:/%'),
    created_at TEXT NOT NULL
) STRICT;

CREATE TABLE roles (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    description TEXT NOT NULL,
    rules_json TEXT NOT NULL CHECK(json_valid(rules_json)),
    system_role TEXT UNIQUE CHECK(system_role IS NULL OR system_role IN ('admin','editor','viewer')),
    managed_by TEXT NOT NULL CHECK(managed_by='user' OR managed_by LIKE 'package:/%'),
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
) STRICT;

CREATE TABLE role_bindings (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    managed_by TEXT NOT NULL CHECK(managed_by='user' OR managed_by LIKE 'package:/%'),
    created_at TEXT NOT NULL
) STRICT;

CREATE TABLE role_binding_role_index (
    role_binding_path TEXT PRIMARY KEY REFERENCES role_bindings(id) ON DELETE CASCADE,
    role_path TEXT NOT NULL REFERENCES roles(id) ON DELETE RESTRICT,
    link_path TEXT NOT NULL UNIQUE REFERENCES links(id) ON DELETE RESTRICT
) STRICT;

CREATE TABLE role_binding_subjects (
    role_binding_path TEXT NOT NULL REFERENCES role_bindings(id) ON DELETE CASCADE,
    subject_kind TEXT NOT NULL CHECK(subject_kind IN ('user','service_account')),
    subject_path TEXT NOT NULL,
    link_path TEXT NOT NULL UNIQUE REFERENCES links(id) ON DELETE RESTRICT,
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

CREATE TABLE links (
    id TEXT PRIMARY KEY,
    source_kind TEXT NOT NULL,
    source_path TEXT NOT NULL,
    relation_path TEXT NOT NULL REFERENCES relations(id) ON DELETE RESTRICT,
    target_kind TEXT NOT NULL,
    target_path TEXT NOT NULL,
    metadata_json TEXT NOT NULL CHECK(json_valid(metadata_json)),
    protected INTEGER NOT NULL DEFAULT 0 CHECK(protected IN (0,1)),
    created_at TEXT NOT NULL,
    UNIQUE(source_kind,source_path,relation_path,target_kind,target_path)
) STRICT;

CREATE INDEX links_source
ON links(source_kind,source_path,relation_path,created_at,id);

CREATE INDEX links_target
ON links(target_kind,target_path,relation_path,created_at,id);

-- Internal projections maintained transactionally with protected ownership
-- Links. They are never exposed as independently mutable API objects.
CREATE TABLE resource_manifest_index (
    resource_path TEXT PRIMARY KEY REFERENCES resources(id) ON DELETE CASCADE,
    manifest_path TEXT NOT NULL REFERENCES manifests(id) ON DELETE RESTRICT,
    link_path TEXT NOT NULL UNIQUE REFERENCES links(id) ON DELETE RESTRICT
) STRICT;

CREATE INDEX resources_by_manifest
ON resource_manifest_index(manifest_path,resource_path);

CREATE TABLE run_relation_index (
    run_path TEXT PRIMARY KEY REFERENCES runs(id) ON DELETE CASCADE,
    resource_path TEXT NOT NULL REFERENCES resources(id) ON DELETE RESTRICT,
    action_path TEXT NOT NULL REFERENCES actions(id) ON DELETE RESTRICT,
    driver_path TEXT NOT NULL REFERENCES drivers(id) ON DELETE RESTRICT,
    resource_link_path TEXT NOT NULL UNIQUE REFERENCES links(id) ON DELETE RESTRICT,
    action_link_path TEXT NOT NULL UNIQUE REFERENCES links(id) ON DELETE RESTRICT,
    driver_link_path TEXT NOT NULL UNIQUE REFERENCES links(id) ON DELETE RESTRICT
) STRICT;

CREATE INDEX queued_runs
ON runs(status,created_at,id);

CREATE INDEX queued_runs_by_driver
ON run_relation_index(driver_path,run_path);

CREATE TABLE events (
    sequence INTEGER PRIMARY KEY AUTOINCREMENT,
    event_type TEXT NOT NULL CHECK(event_type IN ('created','updated','deleted')),
    object_kind TEXT NOT NULL,
    object_path TEXT NOT NULL,
    revision INTEGER CHECK(revision IS NULL OR revision>=0),
    value_json TEXT NOT NULL CHECK(json_valid(value_json)),
    created_at TEXT NOT NULL
) STRICT;

CREATE INDEX events_object_sequence
ON events(object_kind,object_path,sequence);

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
