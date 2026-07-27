-- A Resource is persisted exactly as path + metadata + spec + status.
-- Platform bookkeeping lives under metadata["[kas]"] instead of parallel
-- relational columns.

CREATE TABLE resources_v14 (
    path TEXT PRIMARY KEY,
    metadata TEXT NOT NULL CHECK(json_valid(metadata)),
    spec TEXT NOT NULL CHECK(json_valid(spec)),
    status TEXT NOT NULL CHECK(json_valid(status))
) STRICT;

INSERT INTO resources_v14(path,metadata,spec,status)
SELECT
    path,
    json_object(
        'manifest',manifest_path,
        'name',name,
        'state',state,
        '[kas]',json_object(
            'revision',revision,
            'generation',generation,
            'observed',json(observed_json),
            'protected',json(CASE WHEN protected=1 THEN 'true' ELSE 'false' END),
            'managed_by',managed_by,
            'created_at',created_at,
            'updated_at',updated_at
        )
    ),
    json(spec_json),
    json_set(
        json_remove(status_json,'$.metadata.path'),
        '$.metadata."[kas]".protected',
        json(CASE WHEN protected=1 THEN 'true' ELSE 'false' END),
        '$.metadata."[kas]".managed_by',
        managed_by
    )
FROM resources;

DROP TABLE resources;
ALTER TABLE resources_v14 RENAME TO resources;

CREATE INDEX resources_by_manifest
ON resources(
    json_extract(metadata,'$.manifest'),
    json_extract(metadata,'$."[kas]".created_at'),
    path
);

CREATE UNIQUE INDEX credentials_by_token_hash
ON resources(json_extract(spec,'$.token_hash'))
WHERE json_extract(metadata,'$.manifest')='/builtin/credential';

CREATE UNIQUE INDEX runs_by_request_id
ON resources(json_extract(spec,'$.request_id'))
WHERE json_extract(metadata,'$.manifest')='/builtin/run';

CREATE INDEX runs_by_driver_state
ON resources(
    json_extract(spec,'$.driver'),
    json_extract(status,'$.metadata.state'),
    json_extract(metadata,'$."[kas]".created_at'),
    path
)
WHERE json_extract(metadata,'$.manifest')='/builtin/run';

CREATE INDEX links_by_relation_source
ON resources(
    json_extract(spec,'$.relation'),
    json_extract(spec,'$.source'),
    path
)
WHERE json_extract(metadata,'$.manifest')='/builtin/link';

CREATE INDEX links_by_relation_target
ON resources(
    json_extract(spec,'$.relation'),
    json_extract(spec,'$.target'),
    path
)
WHERE json_extract(metadata,'$.manifest')='/builtin/link';
