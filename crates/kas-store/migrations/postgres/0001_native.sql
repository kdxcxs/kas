CREATE TABLE resources (
    path TEXT PRIMARY KEY,
    metadata JSONB NOT NULL,
    spec JSONB NOT NULL,
    status JSONB NOT NULL
);

CREATE TABLE events (
    sequence BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    event_type TEXT NOT NULL,
    resource_path TEXT NOT NULL,
    revision BIGINT NOT NULL,
    value_json JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL
);

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

CREATE INDEX events_by_resource_sequence
ON events(resource_path,sequence);
