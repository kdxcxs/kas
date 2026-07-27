-- Runtime and lookup projections are derived from Resource documents. KAS now
-- persists only Resources and their operational Events.

ALTER TABLE resources ADD COLUMN generation INTEGER NOT NULL DEFAULT 0
    CHECK(generation >= 0);

UPDATE resources
SET generation = COALESCE((
        SELECT generation FROM driver_runtime
        WHERE driver_runtime.driver_path = resources.path
    ), 0),
    status_json = json_set(
        status_json,
        '$.metadata."[kas]".generation',
        COALESCE((
            SELECT generation FROM driver_runtime
            WHERE driver_runtime.driver_path = resources.path
        ), 0)
    )
WHERE manifest_path = '/builtin/driver';

UPDATE resources
SET spec_json = json_set(
        spec_json,
        '$.token_hash',
        (SELECT token_hash FROM credential_material
         WHERE credential_material.credential_path = resources.path),
        '$.driver_generation',
        (SELECT driver_generation FROM credential_material
         WHERE credential_material.credential_path = resources.path)
    ),
    status_json = json_set(
        status_json,
        '$.spec.token_hash',
        (SELECT token_hash FROM credential_material
         WHERE credential_material.credential_path = resources.path),
        '$.spec.driver_generation',
        (SELECT driver_generation FROM credential_material
         WHERE credential_material.credential_path = resources.path)
    )
WHERE manifest_path = '/builtin/credential'
  AND EXISTS (
      SELECT 1 FROM credential_material
      WHERE credential_material.credential_path = resources.path
  );

UPDATE resources
SET spec_json = json_set(
        spec_json,
        '$.driver_generation',
        (SELECT driver_generation FROM run_runtime
         WHERE run_runtime.run_path = resources.path),
        '$.started_at',
        (SELECT started_at FROM run_runtime
         WHERE run_runtime.run_path = resources.path),
        '$.finished_at',
        (SELECT finished_at FROM run_runtime
         WHERE run_runtime.run_path = resources.path)
    ),
    status_json = json_set(
        status_json,
        '$.spec.driver_generation',
        (SELECT driver_generation FROM run_runtime
         WHERE run_runtime.run_path = resources.path),
        '$.spec.started_at',
        (SELECT started_at FROM run_runtime
         WHERE run_runtime.run_path = resources.path),
        '$.spec.finished_at',
        (SELECT finished_at FROM run_runtime
         WHERE run_runtime.run_path = resources.path)
    )
WHERE manifest_path = '/builtin/run'
  AND EXISTS (
      SELECT 1 FROM run_runtime
      WHERE run_runtime.run_path = resources.path
  );

DROP TABLE driver_deliveries;
DROP TABLE reconcile_queue;
DROP TABLE credential_material;
DROP TABLE driver_service_accounts;
DROP TABLE driver_manifests;
DROP TABLE manifest_packages;
DROP TABLE run_runtime;
DROP TABLE driver_runtime;

CREATE UNIQUE INDEX credentials_by_token_hash
ON resources(json_extract(spec_json, '$.token_hash'))
WHERE manifest_path = '/builtin/credential';

CREATE UNIQUE INDEX runs_by_request_id
ON resources(json_extract(spec_json, '$.request_id'))
WHERE manifest_path = '/builtin/run';

CREATE INDEX runs_by_driver_state
ON resources(
    json_extract(spec_json, '$.driver'),
    json_extract(status_json, '$.metadata.state'),
    created_at,
    path
)
WHERE manifest_path = '/builtin/run';

CREATE INDEX links_by_relation_source
ON resources(
    json_extract(spec_json, '$.relation'),
    json_extract(spec_json, '$.source'),
    path
)
WHERE manifest_path = '/builtin/link';

CREATE INDEX links_by_relation_target
ON resources(
    json_extract(spec_json, '$.relation'),
    json_extract(spec_json, '$.target'),
    path
)
WHERE manifest_path = '/builtin/link';
