-- Resource lifecycle and Driver observations belong to metadata. Business
-- spec/status documents no longer contain the platform state field.

ALTER TABLE resources
ADD COLUMN state TEXT NOT NULL DEFAULT 'available';

ALTER TABLE resources
ADD COLUMN observed_json TEXT NOT NULL DEFAULT '{}' CHECK(json_valid(observed_json));

UPDATE resources
SET state=COALESCE(json_extract(spec_json,'$.state'),'available'),
    spec_json=json_remove(spec_json,'$.state'),
    status_json=json_object(
        'metadata',
        json_object(
            'path',path,
            'manifest',manifest_path,
            'name',name,
            'state',COALESCE(json_extract(status_json,'$.state'),'available'),
            '[kas]',json_object(
                'revision',0,
                'observed',json('{}'),
                'created_at',created_at,
                'updated_at',updated_at
            )
        ),
        'spec',
        json_remove(status_json,'$.state')
    );

-- A Driver belongs to one package Manifest for artifact lookup, but may manage
-- multiple Manifests. Each Manifest still has at most one managing Driver.
CREATE TABLE driver_manifests (
    driver_path TEXT NOT NULL REFERENCES resources(path) ON DELETE CASCADE,
    manifest_path TEXT NOT NULL UNIQUE REFERENCES resources(path) ON DELETE CASCADE,
    PRIMARY KEY(driver_path,manifest_path)
) STRICT;

INSERT INTO driver_manifests(driver_path,manifest_path)
SELECT driver_path,owner_manifest_path FROM driver_runtime;

-- Relation and Link are ordinary Resources. Runtime-critical ownership and
-- authorization mappings are projected directly from their owning Resource
-- specs, so control-plane startup does not depend on a relationship Driver.

CREATE TABLE manifest_packages (
    manifest_path TEXT PRIMARY KEY REFERENCES resources(path) ON DELETE CASCADE,
    package_path TEXT NOT NULL REFERENCES resources(path) ON DELETE RESTRICT
) STRICT;

INSERT INTO manifest_packages(manifest_path,package_path)
SELECT link.target_path,link.source_path
FROM link_index link
JOIN relation_index relation ON relation.relation_path=link.relation_path
WHERE relation.role='package_manifest';

CREATE TABLE driver_service_accounts_v2 (
    driver_path TEXT PRIMARY KEY REFERENCES resources(path) ON DELETE CASCADE,
    service_account_path TEXT NOT NULL UNIQUE
        REFERENCES resources(path) ON DELETE RESTRICT
) STRICT;

INSERT INTO driver_service_accounts_v2(driver_path,service_account_path)
SELECT driver_path,service_account_path FROM driver_service_accounts;

DROP TABLE driver_service_accounts;
ALTER TABLE driver_service_accounts_v2 RENAME TO driver_service_accounts;

CREATE TABLE role_binding_roles_v2 (
    role_binding_path TEXT PRIMARY KEY REFERENCES resources(path) ON DELETE CASCADE,
    role_path TEXT NOT NULL REFERENCES resources(path) ON DELETE RESTRICT
) STRICT;

INSERT INTO role_binding_roles_v2(role_binding_path,role_path)
SELECT role_binding_path,role_path FROM role_binding_roles;

DROP TABLE role_binding_roles;
ALTER TABLE role_binding_roles_v2 RENAME TO role_binding_roles;

CREATE TABLE role_binding_subjects_v2 (
    role_binding_path TEXT NOT NULL REFERENCES resources(path) ON DELETE CASCADE,
    subject_path TEXT NOT NULL REFERENCES resources(path) ON DELETE CASCADE,
    PRIMARY KEY(role_binding_path,subject_path)
) STRICT;

INSERT INTO role_binding_subjects_v2(role_binding_path,subject_path)
SELECT role_binding_path,subject_path FROM role_binding_subjects;

DROP TABLE role_binding_subjects;
ALTER TABLE role_binding_subjects_v2 RENAME TO role_binding_subjects;

CREATE INDEX role_binding_subject_lookup
ON role_binding_subjects(subject_path,role_binding_path);

-- The old reconciliation model could create partial Links. They cannot be
-- decoded by the current /builtin/link Manifest.
DELETE FROM resources
WHERE path IN (
    SELECT link_path FROM link_index
    WHERE source_path IS NULL OR target_path IS NULL
);

DROP TABLE link_index;
DROP TABLE relation_index;

DROP TABLE reconcile_queue;

CREATE TABLE reconcile_queue (
    driver_path TEXT NOT NULL REFERENCES resources(path) ON DELETE CASCADE,
    resource_path TEXT NOT NULL REFERENCES resources(path) ON DELETE CASCADE,
    target_driver_revision INTEGER NOT NULL CHECK(target_driver_revision >= 0),
    target_resource_revision INTEGER NOT NULL CHECK(target_resource_revision >= 0),
    reason TEXT NOT NULL,
    available_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    PRIMARY KEY(driver_path,resource_path)
) STRICT;

CREATE INDEX reconcile_queue_ready
ON reconcile_queue(driver_path,available_at,updated_at,resource_path);
