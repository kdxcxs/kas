CREATE TEMP TABLE role_binding_role_backup AS
SELECT role_binding_path,role_path,link_path FROM role_binding_role_index;

CREATE TEMP TABLE role_binding_subject_backup AS
SELECT role_binding_path,subject_kind,subject_path,link_path FROM role_binding_subjects;

DROP TABLE role_binding_subjects;
DROP TABLE role_binding_role_index;

CREATE TABLE role_bindings_v9 (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    managed_by TEXT NOT NULL CHECK(
        managed_by IN ('user','driver') OR managed_by LIKE 'package:/%'
    ),
    created_at TEXT NOT NULL
) STRICT;

INSERT INTO role_bindings_v9(id,name,managed_by,created_at)
SELECT id,name,managed_by,created_at FROM role_bindings;

DROP TABLE role_bindings;
ALTER TABLE role_bindings_v9 RENAME TO role_bindings;

CREATE TABLE role_binding_role_index (
    role_binding_path TEXT PRIMARY KEY REFERENCES role_bindings(id) ON DELETE CASCADE,
    role_path TEXT NOT NULL REFERENCES roles(id) ON DELETE RESTRICT,
    link_path TEXT NOT NULL UNIQUE REFERENCES links(id) ON DELETE RESTRICT
) STRICT;

CREATE TABLE role_binding_subjects (
    role_binding_path TEXT NOT NULL REFERENCES role_bindings(id) ON DELETE CASCADE,
    subject_kind TEXT NOT NULL CHECK(subject_kind IN ('user','service_account')),
    subject_path TEXT NOT NULL,
    link_path TEXT NOT NULL UNIQUE REFERENCES links(id) ON DELETE RESTRICT,
    PRIMARY KEY(role_binding_path,subject_kind,subject_path)
) STRICT;

INSERT INTO role_binding_role_index(role_binding_path,role_path,link_path)
SELECT role_binding_path,role_path,link_path FROM role_binding_role_backup;

INSERT INTO role_binding_subjects(
    role_binding_path,subject_kind,subject_path,link_path
)
SELECT role_binding_path,subject_kind,subject_path,link_path
FROM role_binding_subject_backup;

DROP TABLE role_binding_role_backup;
DROP TABLE role_binding_subject_backup;

CREATE INDEX role_binding_subject_lookup
ON role_binding_subjects(subject_kind,subject_path,role_binding_path);
