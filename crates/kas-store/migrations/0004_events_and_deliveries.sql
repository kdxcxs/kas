ALTER TABLE manifests RENAME COLUMN driver_name TO driver_name_required;
ALTER TABLE manifests ADD COLUMN driver_name TEXT;
UPDATE manifests SET driver_name=driver_name_required;
ALTER TABLE manifests DROP COLUMN driver_name_required;

DROP TABLE events;
ALTER TABLE runs DROP COLUMN next_event_sequence;

-- Durable, platform-generated object lifecycle log. Business code cannot insert
-- custom events; object writes append one of these rows in the same transaction.
CREATE TABLE events (
    sequence INTEGER PRIMARY KEY AUTOINCREMENT,
    event_type TEXT NOT NULL CHECK(event_type IN ('created','updated','deleted')),
    object_kind TEXT NOT NULL CHECK(object_kind IN ('resource','link','run')),
    object_id TEXT NOT NULL,
    manifest_id TEXT REFERENCES manifests(id) ON DELETE RESTRICT,
    revision INTEGER CHECK(revision IS NULL OR revision>=0),
    value_json TEXT NOT NULL CHECK(json_valid(value_json)),
    created_at TEXT NOT NULL
) STRICT;

CREATE INDEX events_object_sequence
ON events(object_kind,object_id,sequence);

CREATE INDEX events_manifest_sequence
ON events(manifest_id,sequence);

CREATE TABLE driver_deliveries (
    id TEXT PRIMARY KEY,
    driver_id TEXT NOT NULL REFERENCES drivers(id) ON DELETE RESTRICT,
    generation INTEGER NOT NULL CHECK(generation>=0),
    work_json TEXT NOT NULL CHECK(json_valid(work_json)),
    status TEXT NOT NULL CHECK(status IN ('pending','acked','completed')),
    created_at TEXT NOT NULL,
    acked_at TEXT,
    completed_at TEXT
) STRICT;

CREATE INDEX driver_deliveries_replay
ON driver_deliveries(driver_id,generation,status,created_at,id);

INSERT INTO roles(id,name,description,rules_json,managed_by,created_at,updated_at)
SELECT d.id,'system:driver-role:' || d.id,'Driver runtime access',
       '[{"resources":["drivers"],"verbs":["get","patch"]},{"resources":["drivers/connect","drivers/claim"],"verbs":["create"]},{"resources":["resources/status","runs/result"],"verbs":["update"]}]',
       'system',d.created_at,d.updated_at
FROM drivers d;

UPDATE role_bindings
SET role_id=substr(name,length('system:driver:')+1)
WHERE managed_by='system' AND name LIKE 'system:driver:%';
