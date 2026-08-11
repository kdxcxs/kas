CREATE INDEX links_by_relation
ON resources(
    json_extract(spec,'$.relation'),
    path
)
WHERE json_extract(metadata,'$.manifest')='/builtin/link';
