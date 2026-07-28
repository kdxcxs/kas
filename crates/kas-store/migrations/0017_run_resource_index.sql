CREATE INDEX IF NOT EXISTS runs_by_resource
ON resources (
    json_extract(spec,'$.resource'),
    path
)
WHERE json_extract(metadata,'$.manifest')='/builtin/run';
