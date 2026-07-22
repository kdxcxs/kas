CREATE TABLE manifests (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    version INTEGER NOT NULL CHECK(version>0),
    description TEXT NOT NULL,
    resource_schema_json TEXT NOT NULL CHECK(json_valid(resource_schema_json)),
    actions_json TEXT NOT NULL CHECK(json_valid(actions_json)),
    driver_name TEXT NOT NULL,
    created_at TEXT NOT NULL,
    UNIQUE(name,version)
) STRICT;

CREATE TABLE drivers (
    id TEXT PRIMARY KEY,
    manifest_id TEXT NOT NULL UNIQUE REFERENCES manifests(id) ON DELETE RESTRICT,
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
    manifest_id TEXT NOT NULL REFERENCES manifests(id) ON DELETE RESTRICT,
    name TEXT NOT NULL,
    spec_json TEXT NOT NULL CHECK(json_valid(spec_json)),
    status_json TEXT NOT NULL CHECK(json_valid(status_json)),
    revision INTEGER NOT NULL CHECK(revision>=0),
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
) STRICT;

CREATE TABLE runs (
    id TEXT PRIMARY KEY,
    request_id TEXT NOT NULL UNIQUE,
    resource_id TEXT NOT NULL REFERENCES resources(id) ON DELETE RESTRICT,
    driver_id TEXT NOT NULL REFERENCES drivers(id) ON DELETE RESTRICT,
    driver_generation INTEGER,
    action TEXT NOT NULL,
    input_json TEXT NOT NULL CHECK(json_valid(input_json)),
    status TEXT NOT NULL CHECK(status IN ('queued','running','succeeded','failed','cancelled')),
    output_json TEXT CHECK(output_json IS NULL OR json_valid(output_json)),
    error TEXT,
    created_at TEXT NOT NULL,
    started_at TEXT,
    finished_at TEXT,
    next_event_sequence INTEGER NOT NULL CHECK(next_event_sequence>0)
) STRICT;

CREATE INDEX queued_runs ON runs(driver_id,status,created_at,id);

CREATE TABLE events (
    id TEXT NOT NULL UNIQUE,
    run_id TEXT NOT NULL REFERENCES runs(id) ON DELETE RESTRICT,
    sequence INTEGER NOT NULL CHECK(sequence>0),
    kind TEXT NOT NULL,
    data_json TEXT NOT NULL CHECK(json_valid(data_json)),
    created_at TEXT NOT NULL,
    PRIMARY KEY(run_id,sequence)
) STRICT;

CREATE TABLE links (
    id TEXT PRIMARY KEY,
    source_kind TEXT NOT NULL,
    source_id TEXT NOT NULL,
    relation TEXT NOT NULL,
    target_kind TEXT NOT NULL,
    target_id TEXT NOT NULL,
    metadata_json TEXT NOT NULL CHECK(json_valid(metadata_json)),
    created_at TEXT NOT NULL,
    UNIQUE(source_kind,source_id,relation,target_kind,target_id)
) STRICT;
