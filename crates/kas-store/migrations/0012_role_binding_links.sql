-- RoleBinding has no independent lifecycle or state. Represent each
-- Subject-to-Role grant as one ordinary Link using the built-in role-binding
-- Relation.

CREATE TEMP TABLE migrated_role_binding_links AS
SELECT
    binding.path AS binding_path,
    binding.name,
    binding.revision,
    binding.protected,
    binding.managed_by,
    binding.created_at,
    binding.updated_at,
    role.role_path,
    subject.subject_path,
    row_number() OVER (
        PARTITION BY binding.path ORDER BY subject.subject_path
    ) AS subject_number
FROM resources binding
JOIN role_binding_roles role
  ON role.role_binding_path=binding.path
JOIN role_binding_subjects subject
  ON subject.role_binding_path=binding.path
WHERE binding.manifest_path='/builtin/role-binding';

-- Remove the old RoleBinding-to-Role and RoleBinding-to-Subject projections,
-- plus links that describe the removed Manifest and Relations.
DELETE FROM resources
WHERE manifest_path='/builtin/link'
  AND (
      json_extract(spec_json,'$.relation') IN (
          '/builtin/relations/role-binding-role',
          '/builtin/relations/role-binding-subject'
      )
      OR json_extract(spec_json,'$.source') IN (
          '/builtin/role-binding',
          '/builtin/relations/role-binding-role',
          '/builtin/relations/role-binding-subject'
      )
      OR json_extract(spec_json,'$.target') IN (
          '/builtin/role-binding',
          '/builtin/relations/role-binding-role',
          '/builtin/relations/role-binding-subject'
      )
  );

-- Reuse the old RoleBinding path for its first Subject so existing package and
-- dynamically-created paths remain stable.
UPDATE resources
SET manifest_path='/builtin/link',
    spec_json=(
        SELECT json_object(
            'relation','/builtin/relations/role-binding',
            'source',migrated.subject_path,
            'target',migrated.role_path,
            'metadata',json('{}')
        )
        FROM migrated_role_binding_links migrated
        WHERE migrated.binding_path=resources.path
          AND migrated.subject_number=1
    ),
    status_json=json_set(
        status_json,
        '$.metadata.manifest','/builtin/link',
        '$.metadata.state','available',
        '$.spec',json((
            SELECT json_object(
                'relation','/builtin/relations/role-binding',
                'source',migrated.subject_path,
                'target',migrated.role_path,
                'metadata',json('{}')
            )
            FROM migrated_role_binding_links migrated
            WHERE migrated.binding_path=resources.path
              AND migrated.subject_number=1
        ))
    ),
    state='available',
    revision=revision+1
WHERE manifest_path='/builtin/role-binding'
  AND EXISTS (
      SELECT 1
      FROM migrated_role_binding_links migrated
      WHERE migrated.binding_path=resources.path
        AND migrated.subject_number=1
  );

-- A historical RoleBinding could contain multiple Subjects. Additional
-- Subjects become additional Links below the preserved binding path.
INSERT INTO resources(
    path,manifest_path,name,spec_json,status_json,revision,state,observed_json,
    protected,managed_by,created_at,updated_at
)
SELECT
    migrated.binding_path || '/subjects/' || migrated.subject_number,
    '/builtin/link',
    migrated.name || '-' || migrated.subject_number,
    json_object(
        'relation','/builtin/relations/role-binding',
        'source',migrated.subject_path,
        'target',migrated.role_path,
        'metadata',json('{}')
    ),
    json_object(
        'metadata',json_object(
            'path',migrated.binding_path || '/subjects/' || migrated.subject_number,
            'manifest','/builtin/link',
            'name',migrated.name || '-' || migrated.subject_number,
            'state','available',
            '[kas]',json_object(
                'revision',migrated.revision + 1,
                'observed',json('{}'),
                'created_at',migrated.created_at,
                'updated_at',migrated.updated_at
            )
        ),
        'spec',json_object(
            'relation','/builtin/relations/role-binding',
            'source',migrated.subject_path,
            'target',migrated.role_path,
            'metadata',json('{}')
        )
    ),
    migrated.revision + 1,
    'available',
    '{}',
    migrated.protected,
    migrated.managed_by,
    migrated.created_at,
    migrated.updated_at
FROM migrated_role_binding_links migrated
WHERE migrated.subject_number > 1;

-- Invalid empty historical bindings cannot express a relationship.
DELETE FROM resources
WHERE manifest_path='/builtin/role-binding';

DROP TABLE role_binding_subjects;
DROP TABLE role_binding_roles;

DELETE FROM resources
WHERE path IN (
    '/builtin/relations/role-binding-role',
    '/builtin/relations/role-binding-subject'
);

DELETE FROM resources
WHERE path='/builtin/role-binding';

DROP TABLE migrated_role_binding_links;
