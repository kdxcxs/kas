-- Driver deliveries are a durable in-flight queue, not an execution history.
-- Completed work is already reflected in Resources, Runs, observations and
-- Events, so retaining a copy of every completed delivery only grows the
-- database without serving reconnect recovery.

CREATE TABLE driver_deliveries_v11 (
    id TEXT PRIMARY KEY,
    driver_path TEXT NOT NULL REFERENCES resources(path) ON DELETE RESTRICT,
    generation INTEGER NOT NULL CHECK(generation >= 0),
    work_json TEXT NOT NULL CHECK(json_valid(work_json)),
    status TEXT NOT NULL CHECK(status IN ('pending','acked')),
    created_at TEXT NOT NULL,
    acked_at TEXT
) STRICT;

INSERT INTO driver_deliveries_v11(
    id,driver_path,generation,work_json,status,created_at,acked_at
)
SELECT id,driver_path,generation,work_json,status,created_at,acked_at
FROM driver_deliveries
WHERE status IN ('pending','acked');

DROP TABLE driver_deliveries;
ALTER TABLE driver_deliveries_v11 RENAME TO driver_deliveries;

CREATE INDEX driver_deliveries_replay
ON driver_deliveries(driver_path,generation,status,created_at,id);

-- Manifest and Action membership is already represented by
-- resources.manifest_path and covered by resources_by_manifest. These
-- projections had no readers and duplicated values from Resource specs.
DROP TABLE manifest_index;
DROP TABLE action_index;
