CREATE INDEX links_by_source
ON resources(
    json_extract(spec,'$.source'),
    path
)
WHERE json_extract(metadata,'$.manifest')='/builtin/link';

CREATE INDEX links_by_target
ON resources(
    json_extract(spec,'$.target'),
    path
)
WHERE json_extract(metadata,'$.manifest')='/builtin/link';
