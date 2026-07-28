DROP INDEX IF EXISTS resources_by_manifest;
DROP INDEX IF EXISTS credentials_by_token_hash;
DROP INDEX IF EXISTS runs_by_request_id;
DROP INDEX IF EXISTS runs_by_driver_state;
DROP INDEX IF EXISTS runs_by_resource;
DROP INDEX IF EXISTS links_by_relation_source;
DROP INDEX IF EXISTS links_by_relation_target;
DROP INDEX IF EXISTS links_by_source;
DROP INDEX IF EXISTS links_by_target;
DROP INDEX IF EXISTS resources_by_package;
DROP INDEX IF EXISTS resources_by_status_package;

ALTER TABLE resources
    ALTER COLUMN metadata TYPE JSONB USING metadata::JSONB,
    ALTER COLUMN spec TYPE JSONB USING spec::JSONB,
    ALTER COLUMN status TYPE JSONB USING status::JSONB;

ALTER TABLE events
    ALTER COLUMN value_json TYPE JSONB USING value_json::JSONB,
    ALTER COLUMN created_at TYPE TIMESTAMPTZ USING created_at::TIMESTAMPTZ;

CREATE INDEX resources_by_manifest
ON resources (
    (metadata->>'manifest'),
    (metadata#>>'{"[kas]",created_at}'),
    path
);

CREATE UNIQUE INDEX credentials_by_token_hash
ON resources ((spec->>'token_hash'))
WHERE (metadata->>'manifest')='/builtin/credential';

CREATE UNIQUE INDEX runs_by_request_id
ON resources ((spec->>'request_id'))
WHERE (metadata->>'manifest')='/builtin/run';

CREATE INDEX runs_by_driver_state
ON resources (
    (spec->>'driver'),
    (status#>>'{metadata,state}'),
    (metadata#>>'{"[kas]",created_at}'),
    path
)
WHERE (metadata->>'manifest')='/builtin/run';

CREATE INDEX runs_by_resource
ON resources ((spec->>'resource'), path)
WHERE (metadata->>'manifest')='/builtin/run';

CREATE INDEX links_by_relation_source
ON resources (
    (spec->>'relation'),
    (spec->>'source'),
    path
)
WHERE (metadata->>'manifest')='/builtin/link';

CREATE INDEX links_by_relation_target
ON resources (
    (spec->>'relation'),
    (spec->>'target'),
    path
)
WHERE (metadata->>'manifest')='/builtin/link';

CREATE INDEX links_by_source
ON resources ((spec->>'source'), path)
WHERE (metadata->>'manifest')='/builtin/link';

CREATE INDEX links_by_target
ON resources ((spec->>'target'), path)
WHERE (metadata->>'manifest')='/builtin/link';

CREATE INDEX resources_by_package
ON resources ((metadata#>>'{"[kas]",package}'), path);

CREATE INDEX resources_by_status_package
ON resources ((status#>>'{metadata,"[kas]",package}'), path);
