-- Legacy Runs did not record their authenticated Subject, so assigning an
-- owner during migration would fabricate audit data. Remove those records and
-- their protected relationship Links before installing Run Manifest v2.
CREATE TEMP TABLE legacy_run_paths(path TEXT PRIMARY KEY);

INSERT INTO legacy_run_paths(path)
SELECT path
FROM resources
WHERE json_extract(metadata,'$.manifest')='/packages/kas/run/manifest'
  AND json_extract(spec,'$.subject') IS NULL;

CREATE TEMP TABLE legacy_run_link_paths(path TEXT PRIMARY KEY);

INSERT INTO legacy_run_link_paths(path)
SELECT path
FROM resources
WHERE json_extract(metadata,'$.manifest')='/packages/kas/link/manifest'
  AND (
      json_extract(spec,'$.source') IN (SELECT path FROM legacy_run_paths)
      OR json_extract(spec,'$.target') IN (SELECT path FROM legacy_run_paths)
  );

DELETE FROM events
WHERE resource_path IN (SELECT path FROM legacy_run_link_paths)
   OR resource_path IN (SELECT path FROM legacy_run_paths);

DELETE FROM resources WHERE path IN (SELECT path FROM legacy_run_link_paths);
DELETE FROM resources WHERE path IN (SELECT path FROM legacy_run_paths);

DROP TABLE legacy_run_link_paths;
DROP TABLE legacy_run_paths;

DROP INDEX runs_by_request_id;

CREATE INDEX runs_by_subject_action_request
ON resources(
    json_extract(spec,'$.subject'),
    json_extract(spec,'$.action'),
    json_extract(spec,'$.request_id')
)
WHERE json_extract(metadata,'$.manifest')='/packages/kas/run/manifest';
