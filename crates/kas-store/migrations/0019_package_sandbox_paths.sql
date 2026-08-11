DROP INDEX credentials_by_token_hash;
DROP INDEX runs_by_request_id;
DROP INDEX runs_by_driver_state;
DROP INDEX links_by_relation_source;
DROP INDEX links_by_relation_target;
DROP INDEX links_by_source;
DROP INDEX links_by_target;
DROP INDEX runs_by_resource;
DROP INDEX links_by_relation;

CREATE UNIQUE INDEX credentials_by_token_hash
ON resources(json_extract(spec,'$.token_hash'))
WHERE json_extract(metadata,'$.manifest')='/packages/kas/credential/manifest';

CREATE UNIQUE INDEX runs_by_request_id
ON resources(json_extract(spec,'$.request_id'))
WHERE json_extract(metadata,'$.manifest')='/packages/kas/run/manifest';

CREATE INDEX runs_by_driver_state
ON resources(
    json_extract(spec,'$.driver'),
    json_extract(status,'$.metadata.state'),
    json_extract(metadata,'$."[kas]".created_at'),
    path
)
WHERE json_extract(metadata,'$.manifest')='/packages/kas/run/manifest';

CREATE INDEX links_by_relation_source
ON resources(
    json_extract(spec,'$.relation'),
    json_extract(spec,'$.source'),
    path
)
WHERE json_extract(metadata,'$.manifest')='/packages/kas/link/manifest';

CREATE INDEX links_by_relation_target
ON resources(
    json_extract(spec,'$.relation'),
    json_extract(spec,'$.target'),
    path
)
WHERE json_extract(metadata,'$.manifest')='/packages/kas/link/manifest';

CREATE INDEX links_by_source
ON resources(
    json_extract(spec,'$.source'),
    path
)
WHERE json_extract(metadata,'$.manifest')='/packages/kas/link/manifest';

CREATE INDEX links_by_target
ON resources(
    json_extract(spec,'$.target'),
    path
)
WHERE json_extract(metadata,'$.manifest')='/packages/kas/link/manifest';

CREATE INDEX runs_by_resource
ON resources(
    json_extract(spec,'$.resource'),
    path
)
WHERE json_extract(metadata,'$.manifest')='/packages/kas/run/manifest';

CREATE INDEX links_by_relation
ON resources(
    json_extract(spec,'$.relation'),
    path
)
WHERE json_extract(metadata,'$.manifest')='/packages/kas/link/manifest';
