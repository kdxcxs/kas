-- Resource spec/status convergence and first-class Link reconciliation replace
-- revision-observation claims. This is intentionally data-free while the core
-- schema is still unpublished.
DROP TABLE driver_deliveries;
DROP TABLE run_relation_index;
DROP TABLE resource_manifest_index;
DROP TABLE driver_service_account_index;
DROP TABLE role_binding_subjects;
DROP TABLE role_binding_role_index;
DROP TABLE credentials;
DROP TABLE service_accounts;
DROP TABLE links;
DROP TABLE resources;

ALTER TABLE manifests
ADD COLUMN states_json TEXT NOT NULL DEFAULT '[]' CHECK(json_valid(states_json));
ALTER TABLE manifests
ADD COLUMN default_state TEXT NOT NULL DEFAULT 'available';
ALTER TABLE manifests
ADD COLUMN initial_state TEXT NOT NULL DEFAULT 'available';

ALTER TABLE relations
ADD COLUMN relation_type TEXT NOT NULL DEFAULT 'many_to_many'
CHECK(relation_type IN ('one_to_one','one_to_many','many_to_one','many_to_many'));
ALTER TABLE relations
ADD COLUMN ensure INTEGER NOT NULL DEFAULT 0 CHECK(ensure IN (0,1));
ALTER TABLE relations
ADD COLUMN on_source_delete TEXT NOT NULL DEFAULT 'unlink'
CHECK(on_source_delete IN ('unlink','cascade'));
ALTER TABLE relations DROP COLUMN cardinality_json;

CREATE TABLE resources (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    spec_json TEXT NOT NULL CHECK(json_valid(spec_json)),
    status_json TEXT NOT NULL CHECK(json_valid(status_json)),
    revision INTEGER NOT NULL CHECK(revision>=0),
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
) STRICT;

CREATE TABLE links (
    id TEXT PRIMARY KEY,
    source_kind TEXT,
    source_path TEXT,
    relation_path TEXT NOT NULL REFERENCES relations(id) ON DELETE RESTRICT,
    target_kind TEXT,
    target_path TEXT,
    spec_json TEXT NOT NULL CHECK(json_valid(spec_json)),
    status_json TEXT NOT NULL CHECK(json_valid(status_json)),
    metadata_json TEXT NOT NULL CHECK(json_valid(metadata_json)),
    revision INTEGER NOT NULL CHECK(revision>=0),
    protected INTEGER NOT NULL DEFAULT 0 CHECK(protected IN (0,1)),
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    CHECK((source_kind IS NULL)=(source_path IS NULL)),
    CHECK((target_kind IS NULL)=(target_path IS NULL)),
    CHECK(source_path IS NOT NULL OR target_path IS NOT NULL),
    UNIQUE(source_kind,source_path,relation_path,target_kind,target_path)
) STRICT;

CREATE INDEX links_source
ON links(source_kind,source_path,relation_path,created_at,id)
WHERE source_path IS NOT NULL;

CREATE INDEX links_target
ON links(target_kind,target_path,relation_path,created_at,id)
WHERE target_path IS NOT NULL;

CREATE TRIGGER one_to_one_link_insert
BEFORE INSERT ON links
WHEN (SELECT relation_type FROM relations WHERE id=NEW.relation_path)='one_to_one'
BEGIN
    SELECT CASE WHEN NEW.source_path IS NOT NULL AND EXISTS(
        SELECT 1 FROM links
        WHERE relation_path=NEW.relation_path
          AND source_kind=NEW.source_kind
          AND source_path=NEW.source_path
    ) THEN RAISE(ABORT,'one-to-one source is already linked') END;
    SELECT CASE WHEN NEW.target_path IS NOT NULL AND EXISTS(
        SELECT 1 FROM links
        WHERE relation_path=NEW.relation_path
          AND target_kind=NEW.target_kind
          AND target_path=NEW.target_path
    ) THEN RAISE(ABORT,'one-to-one target is already linked') END;
END;

CREATE TRIGGER one_to_one_link_update
BEFORE UPDATE OF source_kind,source_path,target_kind,target_path,relation_path ON links
WHEN (SELECT relation_type FROM relations WHERE id=NEW.relation_path)='one_to_one'
BEGIN
    SELECT CASE WHEN NEW.source_path IS NOT NULL AND EXISTS(
        SELECT 1 FROM links
        WHERE id<>OLD.id
          AND relation_path=NEW.relation_path
          AND source_kind=NEW.source_kind
          AND source_path=NEW.source_path
    ) THEN RAISE(ABORT,'one-to-one source is already linked') END;
    SELECT CASE WHEN NEW.target_path IS NOT NULL AND EXISTS(
        SELECT 1 FROM links
        WHERE id<>OLD.id
          AND relation_path=NEW.relation_path
          AND target_kind=NEW.target_kind
          AND target_path=NEW.target_path
    ) THEN RAISE(ABORT,'one-to-one target is already linked') END;
END;

CREATE TABLE service_accounts (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    managed_by TEXT NOT NULL CHECK(
        managed_by IN ('user','driver') OR managed_by LIKE 'package:/%'
    ),
    created_at TEXT NOT NULL
) STRICT;

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

CREATE TABLE driver_service_account_index (
    driver_path TEXT PRIMARY KEY REFERENCES drivers(id) ON DELETE CASCADE,
    service_account_path TEXT NOT NULL UNIQUE REFERENCES service_accounts(id) ON DELETE RESTRICT,
    link_path TEXT NOT NULL UNIQUE REFERENCES links(id) ON DELETE RESTRICT
) STRICT;

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

CREATE INDEX queued_runs_by_driver
ON run_relation_index(driver_path,run_path);

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

CREATE TABLE reconcile_queue (
    object_kind TEXT NOT NULL CHECK(object_kind IN ('resource','link')),
    object_path TEXT NOT NULL,
    driver_path TEXT NOT NULL REFERENCES drivers(id) ON DELETE CASCADE,
    reason TEXT NOT NULL,
    available_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    PRIMARY KEY(object_kind,object_path)
) STRICT;

CREATE INDEX reconcile_queue_ready
ON reconcile_queue(driver_path,available_at,updated_at,object_kind,object_path);

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
