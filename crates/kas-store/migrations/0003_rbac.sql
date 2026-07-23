CREATE TABLE users (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    disabled INTEGER NOT NULL DEFAULT 0 CHECK(disabled IN (0,1)),
    created_at TEXT NOT NULL
) STRICT;

CREATE TABLE service_accounts (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    driver_id TEXT UNIQUE REFERENCES drivers(id) ON DELETE RESTRICT,
    managed_by TEXT NOT NULL CHECK(managed_by IN ('system','user')),
    created_at TEXT NOT NULL
) STRICT;

CREATE TABLE roles (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    description TEXT NOT NULL,
    rules_json TEXT NOT NULL CHECK(json_valid(rules_json)),
    managed_by TEXT NOT NULL CHECK(managed_by IN ('system','user')),
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
) STRICT;

CREATE TABLE role_bindings (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    role_id TEXT NOT NULL REFERENCES roles(id) ON DELETE RESTRICT,
    managed_by TEXT NOT NULL CHECK(managed_by IN ('system','user')),
    created_at TEXT NOT NULL
) STRICT;

CREATE TABLE role_binding_subjects (
    role_binding_id TEXT NOT NULL REFERENCES role_bindings(id) ON DELETE CASCADE,
    subject_kind TEXT NOT NULL CHECK(subject_kind IN ('user','service_account')),
    subject_id TEXT NOT NULL,
    PRIMARY KEY(role_binding_id,subject_kind,subject_id)
) STRICT;

CREATE INDEX role_binding_subject_lookup
ON role_binding_subjects(subject_kind,subject_id,role_binding_id);

CREATE TABLE credentials (
    id TEXT PRIMARY KEY,
    subject_kind TEXT NOT NULL CHECK(subject_kind IN ('user','service_account')),
    subject_id TEXT NOT NULL,
    token_hash TEXT NOT NULL UNIQUE,
    driver_generation INTEGER,
    expires_at TEXT,
    revoked_at TEXT,
    created_at TEXT NOT NULL
) STRICT;
