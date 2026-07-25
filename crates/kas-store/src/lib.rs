use std::path::Path;

use chrono::{DateTime, Utc};
use kas_auth::{
    AuthContext, CreateRole, CreateRoleBinding, CreateServiceAccount, CreateUser, IssuedCredential,
    Role, RoleBinding, Rule, ServiceAccount, Subject, SubjectKind, User,
};
use kas_core::{
    Action, CreateLink, CreateManifest, CreateResource, CreateRun, DeliveryStatus, Driver,
    DriverDefinition, DriverDelivery, DriverDesiredState, DriverReady, DriverRuntime, DriverState,
    DriverWork, Event, EventFilter, EventType, FinishRun, KindSelector, Link, LinkDirection,
    LinkFilter, Manifest, ManifestDefinition, ManifestRbac, Mutation, ObjectKind, ObjectRef,
    ObjectSelector, OnSourceDelete, PlannedLink, RbacRuleDefinition, RbacSubjectDefinition,
    RbacSubjectKind, ReconcileObject, Relation, RelationRole, RelationType, Resource,
    RestartPolicy, RoleBindingDefinition, RoleDefinition, Run, RunResult, RunStatus,
    ServiceAccountDefinition, SystemRole, UpdateResource, UpdateResourceStatus,
};
use rusqlite::{params, Connection, OptionalExtension, Row, Transaction};
use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("invalid input: {0}")]
    Invalid(String),
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("database migration required: current version {current}, latest version {latest}")]
    MigrationRequired { current: u32, latest: u32 },
    #[error("database schema version {current} is newer than supported version {latest}")]
    UnsupportedSchema { current: u32, latest: u32 },
}

pub const LATEST_SCHEMA_VERSION: u32 = 9;

const MIGRATIONS: &[(u32, &str)] = &[
    (1, include_str!("../migrations/0001_initial.sql")),
    (2, include_str!("../migrations/0002_reconciliations.sql")),
    (3, include_str!("../migrations/0003_rbac.sql")),
    (
        4,
        include_str!("../migrations/0004_events_and_deliveries.sql"),
    ),
    (5, include_str!("../migrations/0005_paths.sql")),
    (6, include_str!("../migrations/0006_link_endpoint_rbac.sql")),
    (
        7,
        include_str!("../migrations/0007_manifest_packages_and_relations.sql"),
    ),
    (
        8,
        include_str!("../migrations/0008_resource_and_link_reconciliation.sql"),
    ),
    (
        9,
        include_str!("../migrations/0009_driver_role_bindings.sql"),
    ),
];

const CORE_BUILTIN: &str = include_str!("../../../builtins/core/manifest.json");
const AUTH_BUILTIN: &str = include_str!("../../../builtins/auth/manifest.json");

pub fn migrate(path: impl AsRef<Path>) -> Result<u32, StoreError> {
    let mut connection = Connection::open(path)?;
    configure(&connection)?;
    migrate_connection(&mut connection)
}

pub struct Store {
    connection: Connection,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriverLaunchConfig {
    pub package_digest: String,
    pub entrypoint: String,
    pub args: Vec<String>,
    pub restart: RestartPolicy,
}

impl Store {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let connection = Connection::open(path)?;
        configure(&connection)?;
        require_current_schema(&connection)?;
        let mut store = Self { connection };
        store.ensure_builtins()?;
        store.reconcile_platform_state()?;
        Ok(store)
    }

    pub fn memory() -> Result<Self, StoreError> {
        let connection = Connection::open_in_memory()?;
        configure(&connection)?;
        let mut store = Self { connection };
        migrate_connection(&mut store.connection)?;
        store.ensure_builtins()?;
        store.reconcile_platform_state()?;
        Ok(store)
    }

    fn ensure_builtins(&mut self) -> Result<(), StoreError> {
        for raw in [CORE_BUILTIN, AUTH_BUILTIN] {
            let hex = Sha256::digest(raw.as_bytes())
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>();
            let digest = format!("sha256:{hex}");
            let definition: ManifestDefinition = serde_json::from_str(raw)?;
            let manifest = definition.resolve(digest).map_err(|error| {
                StoreError::Invalid(format!("invalid built-in Manifest: {error}"))
            })?;
            self.install_manifest(manifest, raw.len() as u64)?;
        }
        Ok(())
    }

    fn reconcile_platform_state(&mut self) -> Result<(), StoreError> {
        let tx = self.connection.transaction()?;
        let now = Utc::now();
        let resources = {
            let mut statement = tx.prepare(
                "SELECT id,name,spec_json,status_json,revision,created_at,updated_at
                 FROM resources ORDER BY created_at,id",
            )?;
            let values = statement
                .query_map([], resource_from_row)?
                .collect::<Result<Vec<_>, _>>()?;
            values
        };
        for original in resources {
            let Some(resource) = tx
                .query_row(RESOURCE_SELECT_BY_ID, [&original.path], resource_from_row)
                .optional()?
            else {
                continue;
            };
            if !finalize_deleted_resource_in_tx(&tx, &resource, now)? {
                enqueue_resource_if_drifted(&tx, &resource, "startup_resync", now)?;
            }
        }
        let links = {
            let mut statement = tx.prepare(
                "SELECT id,source_kind,source_path,relation_path,target_kind,target_path,
                 spec_json,status_json,metadata_json,revision,created_at,updated_at
                 FROM links ORDER BY created_at,id",
            )?;
            let values = statement
                .query_map([], link_from_row)?
                .collect::<Result<Vec<_>, _>>()?;
            values
        };
        for link in links {
            enqueue_link_if_drifted(&tx, &link, "startup_resync", now)?;
        }
        reconcile_ensures_in_tx(&tx, now)?;
        tx.commit()?;
        Ok(())
    }

    pub fn install_manifest(
        &mut self,
        input: CreateManifest,
        package_size: u64,
    ) -> Result<Manifest, StoreError> {
        validate_name("Manifest name", &input.name)?;
        validate_permission_segment("Manifest name", &input.name)?;
        if input.driver.is_none() && !input.actions.is_empty() {
            return Err(StoreError::Invalid(
                "A Manifest that declares Actions must also declare a Driver".into(),
            ));
        }
        if input.driver.is_none() && input.relations.iter().any(|relation| relation.ensure) {
            return Err(StoreError::Invalid(
                "A Manifest that declares an ensured Relation must also declare a Driver".into(),
            ));
        }
        validate_manifest_contract(&input.resource_schema)?;
        validate_manifest_states(&input.states, &input.default_state, &input.initial_state)?;
        validate_package_digest(&input.package_digest)?;
        if input.version == 0 {
            return Err(StoreError::Invalid(
                "Manifest version must start at 1".into(),
            ));
        }
        let now = Utc::now();
        validate_object_path("Manifest path", &input.path)?;
        let manifest_path = input.path.clone();
        let managed_by = format!("package:{manifest_path}");
        let existing: Option<(String, u64)> = self
            .connection
            .query_row(
                "SELECT m.package_digest,p.size_bytes FROM manifests m
                 JOIN packages p ON p.digest=m.package_digest WHERE m.id=?",
                [&manifest_path],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if let Some((digest, size)) = existing {
            if digest == input.package_digest && size == package_size {
                return self.get_manifest(&manifest_path);
            }
            return Err(StoreError::Conflict(format!(
                "Manifest {manifest_path} is already installed from package {digest}"
            )));
        }
        let schema_json = serde_json::to_string(&input.resource_schema)?;
        let tx = self.connection.transaction()?;
        tx.execute(
            "INSERT INTO packages(digest,size_bytes,installed_at) VALUES (?,?,?)
             ON CONFLICT(digest) DO NOTHING",
            params![input.package_digest, package_size, stamp(now)],
        )?;
        let stored_size: u64 = tx.query_row(
            "SELECT size_bytes FROM packages WHERE digest=?",
            [&input.package_digest],
            |row| row.get(0),
        )?;
        if stored_size != package_size {
            return Err(StoreError::Conflict(format!(
                "Package {} was already registered with a different size",
                input.package_digest
            )));
        }
        tx.execute(
            "INSERT INTO manifests(id,name,version,description,resource_schema_json,package_digest,created_at,
             states_json,default_state,initial_state) VALUES (?,?,?,?,?,?,?,?,?,?)",
            params![
                manifest_path.to_string(), input.name, input.version, input.description,
                schema_json, input.package_digest, stamp(now), serde_json::to_string(&input.states)?,
                input.default_state, input.initial_state
            ],
        )
        .map_err(|error| constraint(error, "Manifest name and version already exist"))?;

        let mut members = Vec::new();
        for action in &input.actions {
            validate_manifest_member_path(&manifest_path, "actions", &action.path)?;
            validate_name("Action name", &action.name)?;
            validate_json_schema_contract("Action input schema", &action.input_schema)?;
            validate_json_schema_contract("Action output schema", &action.output_schema)?;
            tx.execute(
                "INSERT INTO actions(id,manifest_path,name,description,input_schema_json,output_schema_json,created_at)
                 VALUES (?,?,?,?,?,?,?)",
                params![
                    action.path,
                    manifest_path,
                    action.name,
                    action.description,
                    serde_json::to_string(&action.input_schema)?,
                    serde_json::to_string(&action.output_schema)?,
                    stamp(now)
                ],
            )?;
            members.push(ObjectRef {
                kind: ObjectKind::Action,
                path: action.path.clone(),
            });
        }
        for relation in &input.relations {
            validate_manifest_member_path(&manifest_path, "relations", &relation.path)?;
            validate_relation_definition(relation)?;
            let metadata_schema = if relation.metadata_schema.is_null() {
                serde_json::json!({})
            } else {
                relation.metadata_schema.clone()
            };
            tx.execute(
                "INSERT INTO relations(id,manifest_path,name,role,inverse_name,sources_json,targets_json,
                 relation_type,ensure,on_source_delete,metadata_schema_json,protected,created_at)
                 VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?)",
                params![
                    relation.path,
                    manifest_path,
                    relation.name,
                    relation.role.map(relation_role),
                    relation.inverse_name,
                    serde_json::to_string(&relation.sources)?,
                    serde_json::to_string(&relation.targets)?,
                    relation_type(relation.relation_type),
                    relation.ensure,
                    on_source_delete(relation.on_source_delete),
                    serde_json::to_string(&metadata_schema)?,
                    relation.role.is_some(),
                    stamp(now)
                ],
            )?;
            members.push(ObjectRef {
                kind: ObjectKind::Relation,
                path: relation.path.clone(),
            });
        }

        for account in &input.rbac.service_accounts {
            validate_manifest_member_path(&manifest_path, "service-accounts", &account.path)?;
            validate_name("ServiceAccount name", &account.name)?;
            tx.execute(
                "INSERT INTO service_accounts(id,name,managed_by,created_at) VALUES (?,?,?,?)",
                params![account.path, account.name, managed_by, stamp(now)],
            )?;
            members.push(ObjectRef {
                kind: ObjectKind::ServiceAccount,
                path: account.path.clone(),
            });
        }
        for role in &input.rbac.roles {
            validate_manifest_member_path(&manifest_path, "roles", &role.path)?;
            validate_name("Role name", &role.name)?;
            let rules = role
                .rules
                .iter()
                .map(|rule| Rule {
                    resources: rule.resources.clone(),
                    verbs: rule.verbs.clone(),
                    paths: rule.paths.clone(),
                })
                .collect::<Vec<_>>();
            validate_rules(&rules)?;
            tx.execute(
                "INSERT INTO roles(id,name,description,rules_json,system_role,managed_by,created_at,updated_at)
                 VALUES (?,?,?,?,?,?,?,?)",
                params![
                    role.path,
                    role.name,
                    role.description,
                    serde_json::to_string(&rules)?,
                    role.system_role.map(system_role),
                    managed_by,
                    stamp(now),
                    stamp(now)
                ],
            )?;
            members.push(ObjectRef {
                kind: ObjectKind::Role,
                path: role.path.clone(),
            });
        }
        for binding in &input.rbac.role_bindings {
            validate_manifest_member_path(&manifest_path, "role-bindings", &binding.path)?;
            validate_name("RoleBinding name", &binding.name)?;
            tx.execute(
                "INSERT INTO role_bindings(id,name,managed_by,created_at) VALUES (?,?,?,?)",
                params![binding.path, binding.name, managed_by, stamp(now)],
            )?;
            members.push(ObjectRef {
                kind: ObjectKind::RoleBinding,
                path: binding.path.clone(),
            });
        }

        if let Some(driver) = &input.driver {
            let expected_driver_path = format!("{manifest_path}/driver");
            if driver.path != expected_driver_path {
                return Err(StoreError::Invalid(format!(
                    "Driver path must be {expected_driver_path}"
                )));
            }
            validate_package_entrypoint(&driver.entrypoint)?;
            tx.execute(
                "INSERT INTO drivers(id,package_digest,runtime,entrypoint,args_json,restart_policy,
                 desired_state,state,generation,process_id,metadata_json,started_at,heartbeat_at,
                 stopped_at,error,created_at,updated_at)
                 VALUES (?,?,'process',?,?,?,'running','stopped',0,NULL,'{}',NULL,NULL,NULL,NULL,?,?)",
                params![
                    driver.path,
                    input.package_digest,
                    driver.entrypoint,
                    serde_json::to_string(&driver.args)?,
                    restart_policy(driver.restart),
                    stamp(now),
                    stamp(now)
                ],
            )?;
            tx.execute(
                "INSERT INTO driver_manifest_index(driver_path,manifest_path) VALUES (?,?)",
                params![driver.path, manifest_path],
            )?;
            members.push(ObjectRef {
                kind: ObjectKind::Driver,
                path: driver.path.clone(),
            });
        }

        let manifest_ref = ObjectRef {
            kind: ObjectKind::Manifest,
            path: manifest_path.clone(),
        };
        for member in members {
            let link_path = format!("{}/links/manifest", member.path);
            insert_protected_link_for_role(
                &tx,
                RelationRole::ManifestMember,
                &link_path,
                manifest_ref.clone(),
                member,
                now,
            )?;
        }
        if let Some(driver) = &input.driver {
            let link_path = format!("{}/links/service-account", driver.path);
            insert_protected_link_for_role(
                &tx,
                RelationRole::DriverServiceAccount,
                &link_path,
                ObjectRef {
                    kind: ObjectKind::Driver,
                    path: driver.path.clone(),
                },
                ObjectRef {
                    kind: ObjectKind::ServiceAccount,
                    path: driver.service_account.clone(),
                },
                now,
            )?;
            tx.execute(
                "INSERT INTO driver_service_account_index(driver_path,service_account_path,link_path)
                 VALUES (?,?,?)",
                params![driver.path, driver.service_account, link_path],
            )?;
        }
        for binding in &input.rbac.role_bindings {
            let role_link_path = format!("{}/links/role", binding.path);
            insert_protected_link_for_role(
                &tx,
                RelationRole::RoleBindingRole,
                &role_link_path,
                ObjectRef {
                    kind: ObjectKind::RoleBinding,
                    path: binding.path.clone(),
                },
                ObjectRef {
                    kind: ObjectKind::Role,
                    path: binding.role_path.clone(),
                },
                now,
            )?;
            tx.execute(
                "INSERT INTO role_binding_role_index(role_binding_path,role_path,link_path)
                 VALUES (?,?,?)",
                params![binding.path, binding.role_path, role_link_path],
            )?;
            for (index, subject) in binding.subjects.iter().enumerate() {
                let subject_link_path = format!("{}/links/subjects/{index}", binding.path);
                let subject_kind = match subject.kind {
                    RbacSubjectKind::User => ObjectKind::User,
                    RbacSubjectKind::ServiceAccount => ObjectKind::ServiceAccount,
                };
                insert_protected_link_for_role(
                    &tx,
                    RelationRole::RoleBindingSubject,
                    &subject_link_path,
                    ObjectRef {
                        kind: ObjectKind::RoleBinding,
                        path: binding.path.clone(),
                    },
                    ObjectRef {
                        kind: subject_kind,
                        path: subject.path.clone(),
                    },
                    now,
                )?;
                tx.execute(
                    "INSERT INTO role_binding_subjects(
                        role_binding_path,subject_kind,subject_path,link_path
                     ) VALUES (?,?,?,?)",
                    params![
                        binding.path,
                        rbac_subject_kind(subject.kind),
                        subject.path,
                        subject_link_path
                    ],
                )?;
            }
        }
        reconcile_ensures_in_tx(&tx, now)?;
        tx.commit()?;
        self.get_manifest(&manifest_path)
    }

    pub fn list_manifests(&self) -> Result<Vec<Manifest>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT id,name,version,description,resource_schema_json,package_digest,created_at
             FROM manifests ORDER BY name,version",
        )?;
        let paths = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        paths
            .iter()
            .map(|path| self.get_manifest(path))
            .collect::<Result<Vec<_>, _>>()
    }

    pub fn get_manifest(&self, path: &str) -> Result<Manifest, StoreError> {
        let (
            path,
            name,
            version,
            description,
            schema,
            package_digest,
            created_at,
            states,
            default_state,
            initial_state,
        ): (
            String,
            String,
            u32,
            String,
            String,
            String,
            String,
            String,
            String,
            String,
        ) = self
            .connection
            .query_row(
                "SELECT id,name,version,description,resource_schema_json,package_digest,created_at,
                 states_json,default_state,initial_state
                 FROM manifests WHERE id=?",
                [path],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                        row.get(7)?,
                        row.get(8)?,
                        row.get(9)?,
                    ))
                },
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound(format!("Manifest {path}")))?;
        let actions = self.list_actions_for_manifest(&path)?;
        let relations = self.list_relations_for_manifest(&path)?;
        let driver = self.driver_definition_for_manifest(&path)?;
        let rbac = self.manifest_rbac(&path)?;
        Ok(Manifest {
            path,
            name,
            version,
            description,
            resource_schema: serde_json::from_str(&schema)?,
            states: serde_json::from_str(&states)?,
            default_state,
            initial_state,
            actions,
            relations,
            driver,
            rbac,
            package_digest,
            created_at: parse_stamp(&created_at)?,
        })
    }

    pub fn get_action(&self, path: &str) -> Result<Action, StoreError> {
        self.connection
            .query_row(
                "SELECT id,name,description,input_schema_json,output_schema_json
                 FROM actions WHERE id=?",
                [path],
                action_from_row,
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound(format!("Action {path}")))
    }

    pub fn get_relation(&self, path: &str) -> Result<Relation, StoreError> {
        self.connection
            .query_row(
                "SELECT id,name,role,inverse_name,sources_json,targets_json,
                 relation_type,ensure,on_source_delete,metadata_schema_json FROM relations WHERE id=?",
                [path],
                relation_from_row,
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound(format!("Relation {path}")))
    }

    pub fn driver_launch_config(
        &self,
        driver_path: &str,
    ) -> Result<DriverLaunchConfig, StoreError> {
        self.connection
            .query_row(
                "SELECT package_digest,entrypoint,args_json,restart_policy FROM drivers WHERE id=?",
                [driver_path],
                |row| {
                    let restart: String = row.get(3)?;
                    Ok(DriverLaunchConfig {
                        package_digest: row.get(0)?,
                        entrypoint: row.get(1)?,
                        args: json_from_row(row, 2)?,
                        restart: restart_policy_from_str(&restart, 3)?,
                    })
                },
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound(format!("Driver {driver_path}")))
    }

    fn list_actions_for_manifest(&self, manifest_path: &str) -> Result<Vec<Action>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT id,name,description,input_schema_json,output_schema_json
             FROM actions WHERE manifest_path=? ORDER BY id",
        )?;
        let actions = statement
            .query_map([manifest_path], action_from_row)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)?;
        Ok(actions)
    }

    fn list_relations_for_manifest(
        &self,
        manifest_path: &str,
    ) -> Result<Vec<Relation>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT id,name,role,inverse_name,sources_json,targets_json,
             relation_type,ensure,on_source_delete,metadata_schema_json
             FROM relations WHERE manifest_path=? ORDER BY id",
        )?;
        let relations = statement
            .query_map([manifest_path], relation_from_row)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)?;
        Ok(relations)
    }

    fn driver_definition_for_manifest(
        &self,
        manifest_path: &str,
    ) -> Result<Option<DriverDefinition>, StoreError> {
        self.connection
            .query_row(
                "SELECT d.id,d.runtime,d.entrypoint,d.args_json,d.restart_policy,
                 s.service_account_path
                 FROM drivers d JOIN driver_manifest_index i ON i.driver_path=d.id
                 JOIN driver_service_account_index s ON s.driver_path=d.id
                 WHERE i.manifest_path=?",
                [manifest_path],
                |row| {
                    let runtime: String = row.get(1)?;
                    let restart: String = row.get(4)?;
                    Ok(DriverDefinition {
                        path: row.get(0)?,
                        runtime: driver_runtime_from_str(&runtime, 1)?,
                        entrypoint: row.get(2)?,
                        service_account: row.get(5)?,
                        args: json_from_row(row, 3)?,
                        restart: restart_policy_from_str(&restart, 4)?,
                    })
                },
            )
            .optional()
            .map_err(StoreError::from)
    }

    fn manifest_rbac(&self, manifest_path: &str) -> Result<ManifestRbac, StoreError> {
        let managed_by = format!("package:{manifest_path}");
        let service_accounts = {
            let mut statement = self
                .connection
                .prepare("SELECT id,name FROM service_accounts WHERE managed_by=? ORDER BY id")?;
            let values = statement
                .query_map([&managed_by], |row| {
                    Ok(ServiceAccountDefinition {
                        path: row.get(0)?,
                        name: row.get(1)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            values
        };
        let roles = {
            let mut statement = self.connection.prepare(
                "SELECT id,name,description,rules_json,system_role
                 FROM roles WHERE managed_by=? ORDER BY id",
            )?;
            let values = statement
                .query_map([&managed_by], |row| {
                    let rules: Vec<Rule> = json_from_row(row, 3)?;
                    let system_role: Option<String> = row.get(4)?;
                    Ok(RoleDefinition {
                        path: row.get(0)?,
                        name: row.get(1)?,
                        description: row.get(2)?,
                        rules: rules
                            .into_iter()
                            .map(|rule| RbacRuleDefinition {
                                resources: rule.resources,
                                verbs: rule.verbs,
                                paths: rule.paths,
                            })
                            .collect(),
                        system_role: system_role
                            .as_deref()
                            .map(|value| system_role_from_str(value, 4))
                            .transpose()?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            values
        };
        let binding_paths = {
            let mut statement = self
                .connection
                .prepare("SELECT id,name FROM role_bindings WHERE managed_by=? ORDER BY id")?;
            let values = statement
                .query_map([&managed_by], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            values
        };
        let mut role_bindings = Vec::new();
        for (path, name) in binding_paths {
            let role_path = self.connection.query_row(
                "SELECT role_path FROM role_binding_role_index WHERE role_binding_path=?",
                [&path],
                |row| row.get(0),
            )?;
            let mut statement = self.connection.prepare(
                "SELECT subject_kind,subject_path FROM role_binding_subjects
                 WHERE role_binding_path=? ORDER BY subject_kind,subject_path",
            )?;
            let subjects = statement
                .query_map([&path], |row| {
                    let kind: String = row.get(0)?;
                    Ok(RbacSubjectDefinition {
                        kind: match kind.as_str() {
                            "user" => RbacSubjectKind::User,
                            "service_account" => RbacSubjectKind::ServiceAccount,
                            other => {
                                return Err(from_sql(0, format!("invalid subject kind {other}")));
                            }
                        },
                        path: row.get(1)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            role_bindings.push(RoleBindingDefinition {
                path,
                name,
                role_path,
                subjects,
            });
        }
        Ok(ManifestRbac {
            service_accounts,
            roles,
            role_bindings,
        })
    }

    pub fn create_resource(&mut self, input: CreateResource) -> Result<Resource, StoreError> {
        validate_object_path("Resource path", &input.path)?;
        validate_name("Resource name", &input.name)?;
        validate_object_path("Manifest path", &input.manifest)?;
        let manifest_path = input.manifest.clone();
        let (schema, states, default_state, initial_state): (String, String, String, String) = self
            .connection
            .query_row(
                "SELECT resource_schema_json,states_json,default_state,initial_state
                 FROM manifests WHERE id=?",
                [&manifest_path],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound(format!("Manifest {manifest_path}")))?;
        let schema: Value = serde_json::from_str(&schema)?;
        let states: Vec<String> = serde_json::from_str(&states)?;
        let spec = resource_document(input.spec, &default_state, &states, false)?;
        let status = resource_document(spec.clone(), &initial_state, &states, true)?;
        validate_json_schema("Resource spec", &schema, &spec)?;
        validate_json_schema("Resource status", &schema, &status)?;
        let now = Utc::now();
        let path = input.path.clone();
        let tx = self.connection.transaction()?;
        tx.execute(
            "INSERT INTO resources(id,name,spec_json,status_json,revision,created_at,updated_at)
                 VALUES (?,?,?,?,0,?,?)",
            params![
                path,
                input.name,
                serde_json::to_string(&spec)?,
                serde_json::to_string(&status)?,
                stamp(now),
                stamp(now)
            ],
        )
        .map_err(|error| constraint(error, "Resource already exists"))?;
        for link in &input.links {
            insert_link(&tx, link, false, now)?;
        }
        let membership_path = format!("{path}/links/manifest");
        insert_protected_link_for_role(
            &tx,
            RelationRole::ResourceManifest,
            &membership_path,
            ObjectRef {
                kind: ObjectKind::Resource,
                path: path.clone(),
            },
            ObjectRef {
                kind: ObjectKind::Manifest,
                path: manifest_path.clone(),
            },
            now,
        )?;
        tx.execute(
            "INSERT INTO resource_manifest_index(resource_path,manifest_path,link_path)
             VALUES (?,?,?)",
            params![path, manifest_path, membership_path],
        )?;
        let resource = Resource {
            path: path.clone(),
            name: input.name,
            spec,
            status,
            revision: 0,
            created_at: now,
            updated_at: now,
        };
        append_lifecycle_event(
            &tx,
            EventType::Created,
            ObjectKind::Resource,
            &path,
            Some(resource.revision),
            &resource,
            now,
        )?;
        enqueue_resource_if_drifted(&tx, &resource, "created", now)?;
        reconcile_ensures_in_tx(&tx, now)?;
        tx.commit()?;
        Ok(resource)
    }

    pub fn update_resource(
        &mut self,
        resource_path: &str,
        input: UpdateResource,
    ) -> Result<Resource, StoreError> {
        let (schema, states): (String, String) = self
            .connection
            .query_row(
                "SELECT m.resource_schema_json,m.states_json FROM resource_manifest_index i
                 JOIN manifests m ON m.id=i.manifest_path WHERE i.resource_path=?",
                [resource_path.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound(format!("Resource {resource_path}")))?;
        let states: Vec<String> = serde_json::from_str(&states)?;
        validate_resource_state(&input.spec, &states)?;
        validate_json_schema(
            "Resource spec",
            &serde_json::from_str(&schema)?,
            &input.spec,
        )?;
        let tx = self.connection.transaction()?;
        let now = Utc::now();
        let changed = tx.execute(
            "UPDATE resources SET spec_json=?,revision=revision+1,updated_at=? WHERE id=? AND revision=?",
            params![
                serde_json::to_string(&input.spec)?,
                stamp(now),
                resource_path.to_string(),
                input.expected_revision
            ],
        )?;
        if changed != 1 {
            return Err(StoreError::Conflict(format!(
                "Resource {resource_path} revision is stale"
            )));
        }
        let resource = tx.query_row(
            RESOURCE_SELECT_BY_ID,
            [resource_path.to_string()],
            resource_from_row,
        )?;
        enqueue_resource_if_drifted(&tx, &resource, "spec_updated", now)?;
        append_lifecycle_event(
            &tx,
            EventType::Updated,
            ObjectKind::Resource,
            resource_path,
            Some(resource.revision),
            &resource,
            now,
        )?;
        tx.commit()?;
        Ok(resource)
    }

    pub fn delete_resource(
        &mut self,
        resource_path: &str,
        expected_revision: u64,
    ) -> Result<Resource, StoreError> {
        let tx = self.connection.transaction()?;
        let mut resource = tx
            .query_row(RESOURCE_SELECT_BY_ID, [resource_path], resource_from_row)
            .optional()?
            .ok_or_else(|| StoreError::NotFound(format!("Resource {resource_path}")))?;
        if resource.revision != expected_revision {
            return Err(StoreError::Conflict(format!(
                "Resource {resource_path} revision is stale"
            )));
        }
        set_document_state(&mut resource.spec, kas_core::STATE_DELETED)?;
        let now = Utc::now();
        tx.execute(
            "UPDATE resources SET spec_json=?,revision=revision+1,updated_at=? WHERE id=?",
            params![
                serde_json::to_string(&resource.spec)?,
                stamp(now),
                resource_path
            ],
        )?;
        resource = tx.query_row(RESOURCE_SELECT_BY_ID, [resource_path], resource_from_row)?;
        append_lifecycle_event(
            &tx,
            EventType::Updated,
            ObjectKind::Resource,
            resource_path,
            Some(resource.revision),
            &resource,
            now,
        )?;
        enqueue_resource_if_drifted(&tx, &resource, "delete_requested", now)?;
        tx.commit()?;
        Ok(resource)
    }

    pub fn list_resources(&self) -> Result<Vec<Resource>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT id,name,spec_json,status_json,revision,created_at,updated_at
             FROM resources ORDER BY created_at,id",
        )?;
        let rows = statement.query_map([], resource_from_row)?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub fn get_resource(&self, resource_path: &str) -> Result<Resource, StoreError> {
        self.connection
            .query_row(
                RESOURCE_SELECT_BY_ID,
                [resource_path.to_string()],
                resource_from_row,
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound(format!("Resource {resource_path}")))
    }

    pub fn manifest_path_for_resource(&self, resource_path: &str) -> Result<String, StoreError> {
        self.connection
            .query_row(
                "SELECT manifest_path FROM resource_manifest_index WHERE resource_path=?",
                [resource_path],
                |row| row.get(0),
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound(format!("Resource {resource_path}")))
    }

    pub fn object_matches_selector(
        &self,
        object: &ObjectRef,
        selector: &ObjectSelector,
    ) -> Result<bool, StoreError> {
        selector_matches(&self.connection, selector, object, 0)
    }

    pub fn links_for_object(&self, object: &ObjectRef) -> Result<Vec<Link>, StoreError> {
        ensure_object_exists(&self.connection, object)?;
        let mut statement = self.connection.prepare(
            "SELECT id,source_kind,source_path,relation_path,target_kind,target_path,
             spec_json,status_json,metadata_json,revision,created_at,updated_at FROM links
             WHERE (source_kind=? AND source_path=?) OR (target_kind=? AND target_path=?)
             ORDER BY created_at,id",
        )?;
        let rows = statement.query_map(
            params![
                object_kind(&object.kind),
                object.path,
                object_kind(&object.kind),
                object.path
            ],
            link_from_row,
        )?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub fn list_objects(&self, kind: Option<ObjectKind>) -> Result<Vec<ObjectRef>, StoreError> {
        let mut objects = all_object_refs(&self.connection)?;
        if let Some(kind) = kind {
            objects.retain(|object| object.kind == kind);
        }
        Ok(objects)
    }

    pub fn object_value(&self, object: &ObjectRef) -> Result<Value, StoreError> {
        ensure_object_exists(&self.connection, object)?;
        let value = match object.kind {
            ObjectKind::Manifest => serde_json::to_value(self.get_manifest(&object.path)?)?,
            ObjectKind::Action => serde_json::to_value(self.get_action(&object.path)?)?,
            ObjectKind::Relation => serde_json::to_value(self.get_relation(&object.path)?)?,
            ObjectKind::Resource => serde_json::to_value(self.get_resource(&object.path)?)?,
            ObjectKind::Driver => serde_json::to_value(self.get_driver(&object.path)?)?,
            ObjectKind::Run => serde_json::to_value(self.get_run(&object.path)?)?,
            ObjectKind::Link => serde_json::to_value(self.get_link(&object.path)?)?,
            ObjectKind::User => serde_json::to_value(self.get_user(&object.path)?)?,
            ObjectKind::ServiceAccount => {
                serde_json::to_value(self.get_service_account(&object.path)?)?
            }
            ObjectKind::Role => serde_json::to_value(self.get_role(&object.path)?)?,
            ObjectKind::RoleBinding => {
                let binding = self
                    .list_role_bindings()?
                    .into_iter()
                    .find(|binding| binding.path == object.path)
                    .ok_or_else(|| {
                        StoreError::NotFound(format!("RoleBinding {}", object.path))
                    })?;
                serde_json::to_value(binding)?
            }
            ObjectKind::Credential => self
                .connection
                .query_row(
                    "SELECT id,subject_kind,subject_path,driver_generation,expires_at,revoked_at,created_at
                     FROM credentials WHERE id=?",
                    [&object.path],
                    |row| {
                        Ok(json!({
                            "path": row.get::<_, String>(0)?,
                            "subject": {
                                "kind": row.get::<_, String>(1)?,
                                "path": row.get::<_, String>(2)?
                            },
                            "driver_generation": row.get::<_, Option<u64>>(3)?,
                            "expires_at": row.get::<_, Option<String>>(4)?,
                            "revoked_at": row.get::<_, Option<String>>(5)?,
                            "created_at": row.get::<_, String>(6)?
                        }))
                    },
                )
                .optional()?
                .ok_or_else(|| StoreError::NotFound(format!("Credential {}", object.path)))?,
        };
        Ok(value)
    }

    pub fn update_resource_status(
        &mut self,
        resource_path: &str,
        input: UpdateResourceStatus,
    ) -> Result<Resource, StoreError> {
        let tx = self.connection.transaction()?;
        let (schema, states): (String, String) = tx
            .query_row(
                "SELECT m.resource_schema_json,m.states_json
                 FROM resource_manifest_index ri JOIN manifests m ON m.id=ri.manifest_path
                 JOIN driver_manifest_index di ON di.manifest_path=m.id
                 JOIN drivers d ON d.id=di.driver_path
                 WHERE ri.resource_path=? AND d.id=? AND d.generation=? AND d.state='ready'",
                params![resource_path, input.driver_path, input.driver_generation],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?
            .ok_or_else(|| {
                StoreError::Conflict("Driver is stale or does not own Resource".into())
            })?;
        let states: Vec<String> = serde_json::from_str(&states)?;
        validate_resource_state(&input.status, &states)?;
        validate_json_schema(
            "Resource status",
            &serde_json::from_str(&schema)?,
            &input.status,
        )?;
        let now = Utc::now();
        let changed = tx.execute(
            "UPDATE resources SET status_json=?,updated_at=? WHERE id=? AND revision=?",
            params![
                serde_json::to_string(&input.status)?,
                stamp(now),
                resource_path,
                input.expected_revision,
            ],
        )?;
        if changed != 1 {
            return Err(StoreError::Conflict("Resource revision is stale".into()));
        }
        let resource = tx.query_row(
            RESOURCE_SELECT_BY_ID,
            [resource_path.to_string()],
            resource_from_row,
        )?;
        append_lifecycle_event(
            &tx,
            EventType::Updated,
            ObjectKind::Resource,
            resource_path,
            Some(resource.revision),
            &resource,
            now,
        )?;
        if !finalize_deleted_resource_in_tx(&tx, &resource, now)? {
            enqueue_resource_if_drifted(&tx, &resource, "status_updated", now)?;
        }
        tx.commit()?;
        Ok(resource)
    }

    pub fn finish_reconciliation_with_mutations(
        &mut self,
        delivery_id: Uuid,
        driver_path: &str,
        generation: u64,
        operations: Vec<Mutation>,
    ) -> Result<Vec<Value>, StoreError> {
        let tx = self.connection.transaction()?;
        let delivery = tx
            .query_row(
                DELIVERY_SELECT_BY_ID,
                [delivery_id.to_string()],
                delivery_from_row,
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound(format!("Driver delivery {delivery_id}")))?;
        if delivery.driver_path != driver_path || delivery.generation != generation {
            return Err(StoreError::Conflict("Delivery is stale".into()));
        }
        let DriverWork::Reconcile { object } = delivery.work else {
            return Err(StoreError::Invalid(
                "Delivery is not reconciliation work".into(),
            ));
        };
        let mut values = Vec::new();
        for operation in &operations {
            match operation {
                Mutation::UpdateResourceStatus {
                    resource_path,
                    expected_revision,
                    status,
                } => {
                    let ReconcileObject::Resource(delivered) = &object else {
                        return Err(StoreError::Invalid(
                            "Resource status cannot complete Link reconciliation".into(),
                        ));
                    };
                    if resource_path != &delivered.path || expected_revision != &delivered.revision
                    {
                        return Err(StoreError::Conflict(
                            "Resource reconciliation mutation does not match delivery".into(),
                        ));
                    }
                    let (schema, states_json): (String, String) = tx.query_row(
                        "SELECT m.resource_schema_json,m.states_json
                         FROM resource_manifest_index ri JOIN manifests m ON m.id=ri.manifest_path
                         WHERE ri.resource_path=?",
                        [resource_path],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )?;
                    let states: Vec<String> = serde_json::from_str(&states_json)?;
                    validate_resource_state(status, &states)?;
                    validate_json_schema(
                        "Resource status",
                        &serde_json::from_str(&schema)?,
                        status,
                    )?;
                    let now = Utc::now();
                    if tx.execute(
                        "UPDATE resources SET status_json=?,updated_at=? WHERE id=? AND revision=?",
                        params![
                            serde_json::to_string(status)?,
                            stamp(now),
                            resource_path,
                            expected_revision
                        ],
                    )? != 1
                    {
                        return Err(StoreError::Conflict("Resource revision is stale".into()));
                    }
                    let resource =
                        tx.query_row(RESOURCE_SELECT_BY_ID, [resource_path], resource_from_row)?;
                    append_lifecycle_event(
                        &tx,
                        EventType::Updated,
                        ObjectKind::Resource,
                        resource_path,
                        Some(resource.revision),
                        &resource,
                        now,
                    )?;
                    if !finalize_deleted_resource_in_tx(&tx, &resource, now)? {
                        enqueue_resource_if_drifted(&tx, &resource, "status_updated", now)?;
                    }
                    values.push(serde_json::to_value(resource)?);
                }
                Mutation::CompleteRun { .. } => {
                    return Err(StoreError::Invalid(
                        "Run completion cannot be used for reconciliation".into(),
                    ));
                }
                other => {
                    apply_mutations(&tx, std::slice::from_ref(other))?;
                    values.push(Value::Null);
                }
            }
        }
        if operations.is_empty() {
            let now = Utc::now();
            match &object {
                ReconcileObject::Resource(resource) => {
                    enqueue_resource_if_drifted(&tx, resource, "reevaluate", now)?
                }
                ReconcileObject::Link(link) => {
                    enqueue_link_if_drifted(&tx, link, "reevaluate", now)?
                }
            }
            tx.execute(
                "UPDATE reconcile_queue SET available_at=? WHERE object_kind=? AND object_path=?",
                params![
                    stamp(now + chrono::Duration::seconds(1)),
                    match &object {
                        ReconcileObject::Resource(_) => "resource",
                        ReconcileObject::Link(_) => "link",
                    },
                    match &object {
                        ReconcileObject::Resource(resource) => resource.path.as_str(),
                        ReconcileObject::Link(link) => link.path.as_str(),
                    }
                ],
            )?;
        }
        complete_delivery_in_tx(&tx, delivery_id, driver_path, generation)?;
        tx.commit()?;
        Ok(values)
    }

    pub fn driver_for_manifest(&self, manifest_path: &str) -> Result<Option<Driver>, StoreError> {
        let manifest_exists: bool = self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM manifests WHERE id=?)",
            [manifest_path.to_string()],
            |row| row.get(0),
        )?;
        if !manifest_exists {
            return Err(StoreError::NotFound(format!("Manifest {manifest_path}")));
        }
        self.connection
            .query_row(
                DRIVER_SELECT_BY_MANIFEST,
                [manifest_path.to_string()],
                driver_from_row,
            )
            .optional()
            .map_err(StoreError::from)
    }

    pub fn get_driver(&self, driver_path: &str) -> Result<Driver, StoreError> {
        self.connection
            .query_row(
                DRIVER_SELECT_BY_ID,
                [driver_path.to_string()],
                driver_from_row,
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound(format!("Driver {driver_path}")))
    }

    pub fn list_drivers(&self) -> Result<Vec<Driver>, StoreError> {
        let mut statement = self
            .connection
            .prepare("SELECT id,desired_state,state,generation,process_id,metadata_json,started_at,heartbeat_at,stopped_at,error,created_at,updated_at FROM drivers ORDER BY id")?;
        let drivers = statement
            .query_map([], driver_from_row)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)?;
        Ok(drivers)
    }

    pub fn start_driver(&mut self, driver_path: &str) -> Result<Driver, StoreError> {
        let current = self.get_driver(driver_path)?;
        if current.desired_state == DriverDesiredState::Running
            && matches!(current.state, DriverState::Starting | DriverState::Ready)
        {
            return Ok(current);
        }
        if current.state == DriverState::Stopping {
            return Err(StoreError::Conflict(
                "Driver cannot start while it is stopping".into(),
            ));
        }
        let now = Utc::now();
        let tx = self.connection.transaction()?;
        let changed = tx.execute(
            "UPDATE drivers SET desired_state='running',state='starting',generation=generation+1,process_id=NULL,
             metadata_json='{}',started_at=?,heartbeat_at=NULL,stopped_at=NULL,error=NULL,updated_at=?
             WHERE id=? AND state IN ('stopped','failed')",
            params![stamp(now), stamp(now), driver_path.to_string()],
        )?;
        if changed != 1 {
            return Err(StoreError::Conflict(
                "Driver can only start from stopped or failed".into(),
            ));
        }
        let running_ids = {
            let mut statement = tx.prepare(
                "SELECT r.id FROM runs r JOIN run_relation_index i ON i.run_path=r.id
                 WHERE i.driver_path=? AND r.status='running'",
            )?;
            let rows = statement
                .query_map([driver_path.to_string()], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            rows
        };
        tx.execute(
            "UPDATE runs SET status='queued',driver_generation=NULL,started_at=NULL
             WHERE id IN (SELECT run_path FROM run_relation_index WHERE driver_path=?)
             AND status='running'",
            [driver_path.to_string()],
        )?;
        for id in running_ids {
            let run = tx.query_row(RUN_SELECT_BY_ID, [&id], run_from_row)?;
            append_lifecycle_event(
                &tx,
                EventType::Updated,
                ObjectKind::Run,
                &id,
                None,
                &run,
                now,
            )?;
        }
        tx.execute(
            "UPDATE driver_deliveries SET status='completed',completed_at=?
             WHERE driver_path=? AND status!='completed'",
            params![stamp(now), driver_path.to_string()],
        )?;
        tx.commit()?;
        self.get_driver(driver_path)
    }

    pub fn mark_driver_ready(
        &mut self,
        driver_path: &str,
        input: DriverReady,
    ) -> Result<Driver, StoreError> {
        let now = Utc::now();
        let metadata = serde_json::to_string(&input.metadata)?;
        let changed = self.connection.execute(
            "UPDATE drivers SET state='ready',process_id=?,metadata_json=?,heartbeat_at=?,updated_at=?
             WHERE id=? AND generation=? AND state='starting'",
            params![input.process_id, metadata, stamp(now), stamp(now), driver_path.to_string(), input.generation],
        )?;
        if changed != 1 {
            return Err(StoreError::Conflict(
                "Driver generation is stale or not starting".into(),
            ));
        }
        self.get_driver(driver_path)
    }

    pub fn heartbeat_driver(
        &mut self,
        driver_path: &str,
        generation: u64,
    ) -> Result<Driver, StoreError> {
        let now = Utc::now();
        let changed = self.connection.execute(
            "UPDATE drivers SET heartbeat_at=?,updated_at=?
             WHERE id=? AND generation=? AND state='ready'",
            params![stamp(now), stamp(now), driver_path.to_string(), generation],
        )?;
        if changed != 1 {
            return Err(StoreError::Conflict(
                "Driver generation is stale or not ready".into(),
            ));
        }
        self.get_driver(driver_path)
    }

    pub fn stop_driver(&mut self, driver_path: &str) -> Result<Driver, StoreError> {
        let now = Utc::now();
        let changed = self.connection.execute(
            "UPDATE drivers SET desired_state='stopped',
             state=CASE WHEN state IN ('starting','ready') THEN 'stopping' ELSE 'stopped' END,
             process_id=CASE WHEN state IN ('starting','ready') THEN process_id ELSE NULL END,
             heartbeat_at=CASE WHEN state IN ('starting','ready') THEN heartbeat_at ELSE NULL END,
             updated_at=?
             WHERE id=?",
            params![stamp(now), driver_path.to_string()],
        )?;
        if changed != 1 {
            return Err(StoreError::NotFound(format!("Driver {driver_path}")));
        }
        self.get_driver(driver_path)
    }

    pub fn mark_driver_failed(
        &mut self,
        driver_path: &str,
        generation: u64,
        error: &str,
    ) -> Result<Driver, StoreError> {
        let now = Utc::now();
        let changed = self.connection.execute(
            "UPDATE drivers SET state='failed',process_id=NULL,heartbeat_at=NULL,error=?,stopped_at=?,updated_at=?
             WHERE id=? AND generation=? AND state IN ('starting','ready','stopping')",
            params![error, stamp(now), stamp(now), driver_path, generation],
        )?;
        if changed != 1 {
            return Err(StoreError::Conflict(
                "Driver generation is stale or already terminal".into(),
            ));
        }
        self.get_driver(driver_path)
    }

    pub fn mark_driver_stopped(
        &mut self,
        driver_path: &str,
        generation: u64,
    ) -> Result<Driver, StoreError> {
        let now = Utc::now();
        let changed = self.connection.execute(
            "UPDATE drivers SET state='stopped',process_id=NULL,heartbeat_at=NULL,stopped_at=?,updated_at=?
             WHERE id=? AND generation=? AND state='stopping'",
            params![stamp(now), stamp(now), driver_path.to_string(), generation],
        )?;
        if changed != 1 {
            return Err(StoreError::Conflict(
                "Driver generation is stale or not stopping".into(),
            ));
        }
        self.get_driver(driver_path)
    }

    pub fn enqueue_run(&mut self, input: CreateRun) -> Result<Run, StoreError> {
        let expected_path = format!("{}/runs/{}", input.resource, input.request_id);
        if input.path != expected_path {
            return Err(StoreError::Invalid(format!(
                "Run path must be {expected_path}"
            )));
        }
        validate_object_path("Run path", &input.path)?;
        let existing = self
            .connection
            .query_row(
                RUN_SELECT_BY_REQUEST,
                [input.request_id.to_string()],
                run_from_row,
            )
            .optional()?;
        if let Some(existing) = existing {
            return Ok(existing);
        }
        let action: Action = self
            .connection
            .query_row(
                "SELECT id,name,description,input_schema_json,output_schema_json
                 FROM actions WHERE id=?",
                [&input.action],
                action_from_row,
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound(format!("Action {}", input.action)))?;
        let driver_path: String = self
            .connection
            .query_row(
                "SELECT di.driver_path
                 FROM resource_manifest_index ri
                 JOIN actions a ON a.manifest_path=ri.manifest_path
                 JOIN driver_manifest_index di ON di.manifest_path=ri.manifest_path
                 WHERE ri.resource_path=? AND a.id=?",
                params![input.resource, input.action],
                |row| row.get(0),
            )
            .optional()?
            .ok_or_else(|| {
                StoreError::Invalid(
                    "Run Resource and Action must belong to one driver-backed Manifest".into(),
                )
            })?;
        let validator = jsonschema::validator_for(&action.input_schema).map_err(|error| {
            StoreError::Invalid(format!(
                "Action {} has an invalid input schema: {error}",
                action.name
            ))
        })?;
        if let Err(error) = validator.validate(&input.input) {
            return Err(StoreError::Invalid(format!(
                "Input for Action {} does not match its schema: {error}",
                action.name
            )));
        }
        let now = Utc::now();
        let path = input.path.clone();
        let tx = self.connection.transaction()?;
        tx.execute(
            "INSERT INTO runs(id,request_id,driver_generation,input_json,status,output_json,error,created_at,started_at,finished_at)
             VALUES (?,?,NULL,?,'queued',NULL,NULL,?,NULL,NULL)",
            params![path, input.request_id.to_string(), serde_json::to_string(&input.input)?, stamp(now)],
        )?;
        for link in &input.links {
            insert_link(&tx, link, false, now)?;
        }
        let resource_link_path = format!("{path}/links/resource");
        let action_link_path = format!("{path}/links/action");
        let driver_link_path = format!("{path}/links/driver");
        insert_protected_link_for_role(
            &tx,
            RelationRole::RunResource,
            &resource_link_path,
            ObjectRef {
                kind: ObjectKind::Run,
                path: path.clone(),
            },
            ObjectRef {
                kind: ObjectKind::Resource,
                path: input.resource.clone(),
            },
            now,
        )?;
        insert_protected_link_for_role(
            &tx,
            RelationRole::RunAction,
            &action_link_path,
            ObjectRef {
                kind: ObjectKind::Run,
                path: path.clone(),
            },
            ObjectRef {
                kind: ObjectKind::Action,
                path: input.action.clone(),
            },
            now,
        )?;
        insert_protected_link_for_role(
            &tx,
            RelationRole::RunDriver,
            &driver_link_path,
            ObjectRef {
                kind: ObjectKind::Run,
                path: path.clone(),
            },
            ObjectRef {
                kind: ObjectKind::Driver,
                path: driver_path.clone(),
            },
            now,
        )?;
        tx.execute(
            "INSERT INTO run_relation_index(
                run_path,resource_path,action_path,driver_path,
                resource_link_path,action_link_path,driver_link_path
             ) VALUES (?,?,?,?,?,?,?)",
            params![
                path,
                input.resource,
                input.action,
                driver_path,
                resource_link_path,
                action_link_path,
                driver_link_path
            ],
        )?;
        let run = tx.query_row(RUN_SELECT_BY_ID, [&path], run_from_row)?;
        append_lifecycle_event(
            &tx,
            EventType::Created,
            ObjectKind::Run,
            &path,
            None,
            &run,
            now,
        )?;
        tx.commit()?;
        Ok(run)
    }

    pub fn claim_run(
        &mut self,
        driver_path: &str,
        generation: u64,
    ) -> Result<Option<Run>, StoreError> {
        let tx = self.connection.transaction()?;
        let ready: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM drivers WHERE id=? AND generation=? AND state='ready')",
            params![driver_path.to_string(), generation],
            |row| row.get(0),
        )?;
        if !ready {
            return Err(StoreError::Conflict("Driver is stale or not ready".into()));
        }
        let run_path: Option<String> = tx
            .query_row(
                "SELECT r.id FROM runs r JOIN run_relation_index i ON i.run_path=r.id
                 WHERE i.driver_path=? AND r.status='queued' ORDER BY r.created_at,r.id LIMIT 1",
                [driver_path.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        let Some(run_path) = run_path else {
            tx.commit()?;
            return Ok(None);
        };
        let now = Utc::now();
        tx.execute(
            "UPDATE runs SET status='running',driver_generation=?,started_at=? WHERE id=? AND status='queued'",
            params![generation, stamp(now), run_path],
        )?;
        let run = tx.query_row(RUN_SELECT_BY_ID, [&run_path], run_from_row)?;
        append_lifecycle_event(
            &tx,
            EventType::Updated,
            ObjectKind::Run,
            &run_path,
            None,
            &run,
            now,
        )?;
        tx.commit()?;
        Ok(Some(run))
    }

    pub fn claim_driver_work(
        &mut self,
        driver_path: &str,
        generation: u64,
    ) -> Result<Option<DriverWork>, StoreError> {
        if let Some(object) = self.claim_reconciliation(driver_path, generation)? {
            return Ok(Some(DriverWork::Reconcile { object }));
        }
        let Some(run) = self.claim_run(driver_path, generation)? else {
            return Ok(None);
        };
        let (resource_path, action_path): (String, String) = self.connection.query_row(
            "SELECT resource_path,action_path FROM run_relation_index WHERE run_path=?",
            [&run.path],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let resource = self.get_resource(&resource_path)?;
        let action = self.get_action(&action_path)?;
        Ok(Some(DriverWork::Run {
            run: Box::new(run),
            resource,
            action,
        }))
    }

    fn claim_reconciliation(
        &mut self,
        driver_path: &str,
        generation: u64,
    ) -> Result<Option<ReconcileObject>, StoreError> {
        let tx = self.connection.transaction()?;
        let ready: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM drivers WHERE id=? AND generation=? AND state='ready')",
            params![driver_path, generation],
            |row| row.get(0),
        )?;
        if !ready {
            return Err(StoreError::Conflict("Driver is stale or not ready".into()));
        }
        let queued: Option<(String, String)> = tx
            .query_row(
                "SELECT object_kind,object_path FROM reconcile_queue
                 WHERE driver_path=? AND available_at<=? ORDER BY available_at,updated_at LIMIT 1",
                params![driver_path, stamp(Utc::now())],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((kind, path)) = queued else {
            tx.commit()?;
            return Ok(None);
        };
        tx.execute(
            "DELETE FROM reconcile_queue WHERE object_kind=? AND object_path=?",
            params![kind, path],
        )?;
        let object = match kind.as_str() {
            "resource" => ReconcileObject::Resource(tx.query_row(
                RESOURCE_SELECT_BY_ID,
                [&path],
                resource_from_row,
            )?),
            "link" => {
                ReconcileObject::Link(tx.query_row(LINK_SELECT_BY_ID, [&path], link_from_row)?)
            }
            _ => {
                return Err(StoreError::Invalid(format!(
                    "invalid reconcile object kind {kind}"
                )))
            }
        };
        tx.commit()?;
        Ok(Some(object))
    }

    pub fn finish_run(&mut self, run_path: &str, input: FinishRun) -> Result<Run, StoreError> {
        self.finish_run_with_mutations(run_path, input, Vec::new())
    }

    pub fn finish_run_with_mutations(
        &mut self,
        run_path: &str,
        input: FinishRun,
        mutations: Vec<Mutation>,
    ) -> Result<Run, StoreError> {
        self.finish_run_internal(run_path, input, mutations, None)
    }

    pub fn finish_run_delivery_with_mutations(
        &mut self,
        delivery_id: Uuid,
        driver_path: &str,
        generation: u64,
        run_path: &str,
        input: FinishRun,
        mutations: Vec<Mutation>,
    ) -> Result<Run, StoreError> {
        self.finish_run_internal(
            run_path,
            input,
            mutations,
            Some((delivery_id, driver_path.to_string(), generation)),
        )
    }

    fn finish_run_internal(
        &mut self,
        run_path: &str,
        input: FinishRun,
        mutations: Vec<Mutation>,
        delivery: Option<(Uuid, String, u64)>,
    ) -> Result<Run, StoreError> {
        let existing = self.get_run(run_path)?;
        if existing.status != RunStatus::Running {
            let same_result = existing.driver_generation == Some(input.driver_generation)
                && match &input.result {
                    RunResult::Succeeded { output } => {
                        existing.status == RunStatus::Succeeded
                            && existing.output.as_ref() == Some(output)
                            && existing.error.is_none()
                    }
                    RunResult::Failed { error } => {
                        existing.status == RunStatus::Failed
                            && existing.error.as_ref() == Some(error)
                            && existing.output.is_none()
                    }
                };
            if same_result {
                if let Some((delivery_id, driver_path, generation)) = delivery {
                    self.complete_driver_delivery(delivery_id, &driver_path, generation)?;
                }
                return Ok(existing);
            }
            return Err(StoreError::Conflict(
                "Run already has a different result".into(),
            ));
        }
        if matches!(&input.result, RunResult::Failed { .. }) && !mutations.is_empty() {
            return Err(StoreError::Invalid(
                "A failed Run cannot commit mutations".into(),
            ));
        }
        let tx = self.connection.transaction()?;
        apply_mutations(&tx, &mutations)?;
        let now = Utc::now();
        let (status, output, error) = match input.result {
            RunResult::Succeeded { output } => {
                ("succeeded", Some(serde_json::to_string(&output)?), None)
            }
            RunResult::Failed { error } => ("failed", None, Some(error)),
        };
        let changed = tx.execute(
            "UPDATE runs SET status=?,output_json=?,error=?,finished_at=?
             WHERE id=? AND status='running' AND driver_generation=?
             AND EXISTS(
                SELECT 1 FROM run_relation_index i
                JOIN drivers d ON d.id=i.driver_path
                WHERE i.run_path=runs.id AND d.generation=? AND d.state='ready'
             )",
            params![
                status,
                output,
                error,
                stamp(now),
                run_path.to_string(),
                input.driver_generation,
                input.driver_generation
            ],
        )?;
        if changed != 1 {
            return Err(StoreError::Conflict(
                "Run is not owned by the current Driver generation".into(),
            ));
        }
        let run = tx.query_row(RUN_SELECT_BY_ID, [run_path.to_string()], run_from_row)?;
        append_lifecycle_event(
            &tx,
            EventType::Updated,
            ObjectKind::Run,
            run_path,
            None,
            &run,
            now,
        )?;
        if let Some((delivery_id, driver_path, generation)) = delivery {
            complete_delivery_in_tx(&tx, delivery_id, &driver_path, generation)?;
        }
        tx.commit()?;
        Ok(run)
    }

    pub fn get_run(&self, run_path: &str) -> Result<Run, StoreError> {
        self.connection
            .query_row(RUN_SELECT_BY_ID, [run_path.to_string()], run_from_row)
            .optional()?
            .ok_or_else(|| StoreError::NotFound(format!("Run {run_path}")))
    }

    pub fn create_link(&mut self, input: CreateLink) -> Result<Link, StoreError> {
        validate_object_path("Link path", &input.path)?;
        let path = input.path.clone();
        let now = Utc::now();
        let tx = self.connection.transaction()?;
        let planned = kas_core::PlannedLink {
            path: input.path,
            source: input.source,
            relation_path: input.relation_path,
            target: input.target,
            spec: input.spec,
            status: input.status,
            metadata: input.metadata,
        };
        insert_link(&tx, &planned, false, now)?;
        let link = tx.query_row(LINK_SELECT_BY_ID, [&path], link_from_row)?;
        append_lifecycle_event(
            &tx,
            EventType::Created,
            ObjectKind::Link,
            &path,
            None,
            &link,
            now,
        )?;
        enqueue_link_if_drifted(&tx, &link, "created", now)?;
        reconcile_ensures_in_tx(&tx, now)?;
        tx.commit()?;
        Ok(link)
    }

    pub fn get_link(&self, path: &str) -> Result<Link, StoreError> {
        self.connection
            .query_row(LINK_SELECT_BY_ID, [path], link_from_row)
            .optional()?
            .ok_or_else(|| StoreError::NotFound(format!("Link {path}")))
    }

    pub fn list_links(&self, filter: LinkFilter) -> Result<Vec<Link>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT id,source_kind,source_path,relation_path,target_kind,target_path,
             spec_json,status_json,metadata_json,revision,created_at,updated_at
             FROM links ORDER BY created_at,id",
        )?;
        let rows = statement.query_map([], link_from_row)?;
        let links = rows.collect::<Result<Vec<_>, _>>()?;
        Ok(links
            .into_iter()
            .filter(|link| {
                filter
                    .source
                    .as_ref()
                    .is_none_or(|value| link.source.as_ref() == Some(value))
                    && filter
                        .relation_path
                        .as_ref()
                        .is_none_or(|value| value == &link.relation_path)
                    && filter
                        .target
                        .as_ref()
                        .is_none_or(|value| link.target.as_ref() == Some(value))
            })
            .collect())
    }

    pub fn delete_link(&mut self, path: &str) -> Result<(), StoreError> {
        let tx = self.connection.transaction()?;
        let link = tx
            .query_row(LINK_SELECT_BY_ID, [path], link_from_row)
            .optional()?
            .ok_or_else(|| StoreError::NotFound(format!("Link {path}")))?;
        let protected: bool =
            tx.query_row("SELECT protected FROM links WHERE id=?", [path], |row| {
                row.get(0)
            })?;
        if protected {
            return Err(StoreError::Invalid(format!(
                "System Link {path} cannot be deleted independently"
            )));
        }
        if tx.execute("DELETE FROM links WHERE id=?", [path])? != 1 {
            return Err(StoreError::NotFound(format!("Link {path}")));
        }
        append_lifecycle_event(
            &tx,
            EventType::Deleted,
            ObjectKind::Link,
            path,
            None,
            &link,
            Utc::now(),
        )?;
        reconcile_ensures_in_tx(&tx, Utc::now())?;
        tx.commit()?;
        Ok(())
    }

    pub fn list_events(
        &self,
        after_sequence: Option<u64>,
        limit: usize,
    ) -> Result<Vec<Event>, StoreError> {
        self.list_events_filtered(EventFilter {
            after_sequence,
            limit: Some(limit),
            ..Default::default()
        })
    }

    pub fn current_event_sequence(&self) -> Result<u64, StoreError> {
        self.connection
            .query_row("SELECT COALESCE(MAX(sequence),0) FROM events", [], |row| {
                row.get(0)
            })
            .map_err(StoreError::from)
    }

    pub fn list_events_filtered(&self, filter: EventFilter) -> Result<Vec<Event>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT sequence,event_type,object_kind,object_path,revision,value_json,created_at
             FROM events WHERE (?1 IS NULL OR object_kind=?1)
             AND (?2 IS NULL OR object_path=?2)
             AND sequence>?3 ORDER BY sequence LIMIT ?4",
        )?;
        let rows = statement.query_map(
            params![
                filter.object_kind.as_ref().map(object_kind),
                filter.object_path.map(|id| id.to_string()),
                filter.after_sequence.unwrap_or(0),
                filter.limit.unwrap_or(100).clamp(1, 1000)
            ],
            event_from_row,
        )?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub fn pending_driver_deliveries(
        &self,
        driver_path: &str,
        generation: u64,
    ) -> Result<Vec<DriverDelivery>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT id,driver_path,generation,work_json,status,created_at,acked_at,completed_at
             FROM driver_deliveries WHERE driver_path=? AND generation=? AND status!='completed'
             ORDER BY created_at,id",
        )?;
        let rows = statement.query_map(
            params![driver_path.to_string(), generation],
            delivery_from_row,
        )?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub fn claim_driver_delivery(
        &mut self,
        driver_path: &str,
        generation: u64,
    ) -> Result<Option<DriverDelivery>, StoreError> {
        if let Some(delivery) = self
            .pending_driver_deliveries(driver_path, generation)?
            .into_iter()
            .next()
        {
            return Ok(Some(delivery));
        }
        let Some(work) = self.claim_driver_work(driver_path, generation)? else {
            return Ok(None);
        };
        let id = Uuid::new_v4();
        let now = Utc::now();
        self.connection.execute(
            "INSERT INTO driver_deliveries(id,driver_path,generation,work_json,status,created_at)
             VALUES (?,?,?,?,'pending',?)",
            params![
                id.to_string(),
                driver_path.to_string(),
                generation,
                serde_json::to_string(&work)?,
                stamp(now)
            ],
        )?;
        Ok(Some(DriverDelivery {
            id,
            driver_path: driver_path.to_string(),
            generation,
            work,
            status: DeliveryStatus::Pending,
            created_at: now,
            acked_at: None,
            completed_at: None,
        }))
    }

    pub fn acknowledge_driver_delivery(
        &mut self,
        id: Uuid,
        driver_path: &str,
        generation: u64,
    ) -> Result<DriverDelivery, StoreError> {
        let now = Utc::now();
        let changed = self.connection.execute(
            "UPDATE driver_deliveries SET status='acked',acked_at=?
             WHERE id=? AND driver_path=? AND generation=? AND status='pending'",
            params![
                stamp(now),
                id.to_string(),
                driver_path.to_string(),
                generation
            ],
        )?;
        if changed == 0 {
            let existing = self.get_driver_delivery(id)?;
            if existing.driver_path == driver_path
                && existing.generation == generation
                && existing.status == DeliveryStatus::Acked
            {
                return Ok(existing);
            }
            return Err(StoreError::Conflict(
                "Delivery is stale or already completed".into(),
            ));
        }
        self.get_driver_delivery(id)
    }

    pub fn complete_driver_delivery(
        &mut self,
        id: Uuid,
        driver_path: &str,
        generation: u64,
    ) -> Result<DriverDelivery, StoreError> {
        let changed = self.connection.execute(
            "UPDATE driver_deliveries SET status='completed',completed_at=?
             WHERE id=? AND driver_path=? AND generation=? AND status!='completed'",
            params![
                stamp(Utc::now()),
                id.to_string(),
                driver_path.to_string(),
                generation
            ],
        )?;
        if changed == 0 {
            let existing = self.get_driver_delivery(id)?;
            if existing.driver_path == driver_path
                && existing.generation == generation
                && existing.status == DeliveryStatus::Completed
            {
                return Ok(existing);
            }
            return Err(StoreError::Conflict("Delivery is stale".into()));
        }
        self.get_driver_delivery(id)
    }

    pub fn get_driver_delivery(&self, id: Uuid) -> Result<DriverDelivery, StoreError> {
        self.connection
            .query_row(DELIVERY_SELECT_BY_ID, [id.to_string()], delivery_from_row)
            .optional()?
            .ok_or_else(|| StoreError::NotFound(format!("Driver delivery {id}")))
    }

    pub fn bootstrap_admin(&mut self, name: &str) -> Result<IssuedCredential, StoreError> {
        validate_name("User name", name)?;
        let existing: bool =
            self.connection
                .query_row("SELECT EXISTS(SELECT 1 FROM users)", [], |row| row.get(0))?;
        if existing {
            return Err(StoreError::Conflict(
                "An administrator is already bootstrapped".into(),
            ));
        }
        let now = Utc::now();
        let user_path = format!("/users/{name}");
        validate_object_path("Bootstrap User path", &user_path)?;
        let binding_path = "/role-bindings/system/bootstrap-admin".to_string();
        let admin_role_path: String = self
            .connection
            .query_row(
                "SELECT id FROM roles WHERE system_role='admin'",
                [],
                |row| row.get(0),
            )
            .optional()?
            .ok_or_else(|| StoreError::Invalid("built-in admin Role is not installed".into()))?;
        let token = kas_auth::issue_token();
        let credential_path = format!("{user_path}/credentials/{}", Uuid::new_v4());
        let tx = self.connection.transaction()?;
        tx.execute(
            "INSERT INTO users(id,name,disabled,created_at) VALUES (?,?,0,?)",
            params![user_path.to_string(), name, stamp(now)],
        )?;
        tx.execute(
            "INSERT INTO role_bindings(id,name,managed_by,created_at) VALUES (?,?,'user',?)",
            params![binding_path, "system:bootstrap-admin", stamp(now)],
        )?;
        let role_link_path = format!("{binding_path}/links/role");
        insert_protected_link_for_role(
            &tx,
            RelationRole::RoleBindingRole,
            &role_link_path,
            ObjectRef {
                kind: ObjectKind::RoleBinding,
                path: binding_path.clone(),
            },
            ObjectRef {
                kind: ObjectKind::Role,
                path: admin_role_path.clone(),
            },
            now,
        )?;
        tx.execute(
            "INSERT INTO role_binding_role_index(role_binding_path,role_path,link_path)
             VALUES (?,?,?)",
            params![binding_path, admin_role_path, role_link_path],
        )?;
        let subject_link_path = format!("{binding_path}/links/subjects/0");
        insert_protected_link_for_role(
            &tx,
            RelationRole::RoleBindingSubject,
            &subject_link_path,
            ObjectRef {
                kind: ObjectKind::RoleBinding,
                path: binding_path.clone(),
            },
            ObjectRef {
                kind: ObjectKind::User,
                path: user_path.clone(),
            },
            now,
        )?;
        tx.execute(
            "INSERT INTO role_binding_subjects(
                role_binding_path,subject_kind,subject_path,link_path
             ) VALUES (?,'user',?,?)",
            params![binding_path, user_path, subject_link_path],
        )?;
        tx.execute(
            "INSERT INTO credentials(id,subject_kind,subject_path,token_hash,driver_generation,expires_at,revoked_at,created_at)
             VALUES (?,'user',?,?,NULL,NULL,NULL,?)",
            params![credential_path.to_string(), user_path.to_string(), kas_auth::token_hash(&token), stamp(now)],
        )?;
        tx.commit()?;
        Ok(IssuedCredential {
            path: credential_path,
            token,
            expires_at: None,
        })
    }

    pub fn authenticate(&self, token: &str) -> Result<AuthContext, StoreError> {
        let hash = kas_auth::token_hash(token);
        let now = stamp(Utc::now());
        let row: Option<(String, String, Option<u64>, Option<String>)> = self.connection
            .query_row(
                "SELECT c.subject_kind,c.subject_path,c.driver_generation,dsi.driver_path
                 FROM credentials c
                 LEFT JOIN users u ON c.subject_kind='user' AND u.id=c.subject_path
                 LEFT JOIN service_accounts sa ON c.subject_kind='service_account' AND sa.id=c.subject_path
                 LEFT JOIN driver_service_account_index dsi ON dsi.service_account_path=sa.id
                 WHERE c.token_hash=? AND c.revoked_at IS NULL
                 AND (c.expires_at IS NULL OR c.expires_at>?)
                 AND ((c.subject_kind='user' AND u.disabled=0) OR (c.subject_kind='service_account' AND sa.id IS NOT NULL))",
                params![hash, now],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        let Some((kind, subject_path, driver_generation, driver_path)) = row else {
            return Err(StoreError::Invalid("Invalid or expired credential".into()));
        };
        let kind = parse_subject_kind(&kind)?;
        if let (Some(driver_path), Some(generation)) = (&driver_path, driver_generation) {
            let driver = self.get_driver(driver_path)?;
            if driver.generation != generation {
                return Err(StoreError::Invalid(
                    "Driver credential generation is stale".into(),
                ));
            }
        }
        let mut statement = self.connection.prepare(
            "SELECT r.rules_json FROM roles r
             JOIN role_binding_role_index rbri ON rbri.role_path=r.id
             JOIN role_bindings rb ON rb.id=rbri.role_binding_path
             JOIN role_binding_subjects rbs ON rbs.role_binding_path=rb.id
             WHERE rbs.subject_kind=? AND rbs.subject_path=?",
        )?;
        let rows = statement
            .query_map(params![kind.as_str(), subject_path.to_string()], |row| {
                row.get::<_, String>(0)
            })?;
        let mut rules = Vec::new();
        for row in rows {
            rules.extend(serde_json::from_str::<Vec<Rule>>(&row?)?);
        }
        Ok(AuthContext {
            subject: Subject {
                kind,
                path: subject_path,
            },
            rules,
            driver_path,
            driver_generation,
        })
    }

    pub fn issue_driver_credential(
        &mut self,
        driver_path: &str,
    ) -> Result<IssuedCredential, StoreError> {
        let driver = self.get_driver(driver_path)?;
        if driver.state != DriverState::Starting {
            return Err(StoreError::Conflict(
                "Driver must be starting before credentials are issued".into(),
            ));
        }
        let service_account_path: String = self.connection.query_row(
            "SELECT service_account_path FROM driver_service_account_index WHERE driver_path=?",
            [driver_path.to_string()],
            |row| row.get(0),
        )?;
        self.issue_credential(
            Subject {
                kind: SubjectKind::ServiceAccount,
                path: service_account_path,
            },
            Some(driver.generation),
            Some(Utc::now() + chrono::Duration::hours(1)),
            true,
        )
    }

    pub fn issue_user_credential(
        &mut self,
        user_path: &str,
    ) -> Result<IssuedCredential, StoreError> {
        self.get_user(user_path)?;
        self.issue_credential(
            Subject {
                kind: SubjectKind::User,
                path: user_path.to_string(),
            },
            None,
            None,
            false,
        )
    }

    pub fn issue_service_account_credential(
        &mut self,
        service_account_path: &str,
    ) -> Result<IssuedCredential, StoreError> {
        let is_driver: bool = self.connection.query_row(
            "SELECT EXISTS(
                    SELECT 1 FROM service_accounts sa
                    LEFT JOIN driver_service_account_index dsi ON dsi.service_account_path=sa.id
                    WHERE sa.id=? AND dsi.driver_path IS NOT NULL
                 )",
            [service_account_path.to_string()],
            |row| row.get(0),
        )?;
        self.get_service_account(service_account_path)?;
        if is_driver {
            return Err(StoreError::Conflict(
                "Driver credentials must be issued through the Driver endpoint".into(),
            ));
        }
        self.issue_credential(
            Subject {
                kind: SubjectKind::ServiceAccount,
                path: service_account_path.to_string(),
            },
            None,
            None,
            false,
        )
    }

    fn issue_credential(
        &mut self,
        subject: Subject,
        driver_generation: Option<u64>,
        expires_at: Option<DateTime<Utc>>,
        revoke_existing: bool,
    ) -> Result<IssuedCredential, StoreError> {
        let now = Utc::now();
        let path = format!("{}/credentials/{}", subject.path, Uuid::new_v4());
        let token = kas_auth::issue_token();
        let tx = self.connection.transaction()?;
        if revoke_existing {
            tx.execute(
                "UPDATE credentials SET revoked_at=? WHERE subject_kind=? AND subject_path=? AND revoked_at IS NULL",
                params![stamp(now), subject.kind.as_str(), subject.path.to_string()],
            )?;
        }
        tx.execute(
            "INSERT INTO credentials(id,subject_kind,subject_path,token_hash,driver_generation,expires_at,revoked_at,created_at)
             VALUES (?,?,?,?,?,?,NULL,?)",
            params![path, subject.kind.as_str(), subject.path, kas_auth::token_hash(&token), driver_generation, expires_at.map(stamp), stamp(now)],
        )?;
        tx.commit()?;
        Ok(IssuedCredential {
            path,
            token,
            expires_at,
        })
    }

    pub fn revoke_credential(&mut self, path: &str) -> Result<(), StoreError> {
        validate_object_path("Credential path", path)?;
        let changed = self.connection.execute(
            "UPDATE credentials SET revoked_at=? WHERE id=? AND revoked_at IS NULL",
            params![stamp(Utc::now()), path],
        )?;
        if changed == 1 {
            return Ok(());
        }
        let exists: bool = self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM credentials WHERE id=?)",
            [path],
            |row| row.get(0),
        )?;
        if exists {
            return Ok(());
        }
        Err(StoreError::NotFound(format!("Credential {path}")))
    }

    pub fn create_user(&mut self, input: CreateUser) -> Result<User, StoreError> {
        validate_name("User name", &input.name)?;
        validate_object_path("User path", &input.path)?;
        let now = Utc::now();
        self.connection
            .execute(
                "INSERT INTO users(id,name,disabled,created_at) VALUES (?,?,0,?)",
                params![input.path, input.name, stamp(now)],
            )
            .map_err(|error| constraint(error, "User name already exists"))?;
        Ok(User {
            path: input.path,
            name: input.name,
            disabled: false,
            created_at: now,
        })
    }

    pub fn get_user(&self, path: &str) -> Result<User, StoreError> {
        self.connection
            .query_row(
                "SELECT id,name,disabled,created_at FROM users WHERE id=?",
                [path],
                user_from_row,
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound(format!("User {path}")))
    }

    pub fn list_users(&self) -> Result<Vec<User>, StoreError> {
        let mut statement = self
            .connection
            .prepare("SELECT id,name,disabled,created_at FROM users ORDER BY name")?;
        let rows = statement.query_map([], user_from_row)?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub fn create_service_account(
        &mut self,
        input: CreateServiceAccount,
    ) -> Result<ServiceAccount, StoreError> {
        validate_name("ServiceAccount name", &input.name)?;
        validate_object_path("ServiceAccount path", &input.path)?;
        let now = Utc::now();
        let tx = self.connection.transaction()?;
        tx.execute(
            "INSERT INTO service_accounts(id,name,managed_by,created_at) VALUES (?,?,'user',?)",
            params![input.path, input.name, stamp(now)],
        )
        .map_err(|error| constraint(error, "ServiceAccount name already exists"))?;
        reconcile_ensures_in_tx(&tx, now)?;
        tx.commit()?;
        Ok(ServiceAccount {
            path: input.path,
            name: input.name,
            driver_path: None,
            managed_by: "user".into(),
            created_at: now,
        })
    }

    pub fn list_service_accounts(&self) -> Result<Vec<ServiceAccount>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT sa.id,sa.name,dsi.driver_path,sa.managed_by,sa.created_at
             FROM service_accounts sa
             LEFT JOIN driver_service_account_index dsi ON dsi.service_account_path=sa.id
             ORDER BY sa.name",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(ServiceAccount {
                path: row.get(0)?,
                name: row.get(1)?,
                driver_path: row.get(2)?,
                managed_by: row.get(3)?,
                created_at: time_from_row(row, 4)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub fn get_service_account(&self, path: &str) -> Result<ServiceAccount, StoreError> {
        self.connection
            .query_row(
                "SELECT sa.id,sa.name,dsi.driver_path,sa.managed_by,sa.created_at
                 FROM service_accounts sa
                 LEFT JOIN driver_service_account_index dsi ON dsi.service_account_path=sa.id
                 WHERE sa.id=?",
                [path],
                |row| {
                    Ok(ServiceAccount {
                        path: row.get(0)?,
                        name: row.get(1)?,
                        driver_path: row.get(2)?,
                        managed_by: row.get(3)?,
                        created_at: time_from_row(row, 4)?,
                    })
                },
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound(format!("ServiceAccount {path}")))
    }

    pub fn create_role(&mut self, input: CreateRole) -> Result<Role, StoreError> {
        validate_name("Role name", &input.name)?;
        validate_object_path("Role path", &input.path)?;
        validate_rules(&input.rules)?;
        let now = Utc::now();
        self.connection.execute(
            "INSERT INTO roles(id,name,description,rules_json,system_role,managed_by,created_at,updated_at)
             VALUES (?,?,?,?,NULL,'user',?,?)",
            params![input.path, input.name, input.description, serde_json::to_string(&input.rules)?, stamp(now), stamp(now)],
        ).map_err(|error| constraint(error, "Role name already exists"))?;
        Ok(Role {
            path: input.path,
            name: input.name,
            description: input.description,
            rules: input.rules,
            managed_by: "user".into(),
            created_at: now,
            updated_at: now,
        })
    }

    pub fn list_roles(&self) -> Result<Vec<Role>, StoreError> {
        let mut statement = self.connection.prepare("SELECT id,name,description,rules_json,managed_by,created_at,updated_at FROM roles ORDER BY name")?;
        let rows = statement.query_map([], role_from_row)?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub fn get_role(&self, path: &str) -> Result<Role, StoreError> {
        self.connection
            .query_row(
                "SELECT id,name,description,rules_json,managed_by,created_at,updated_at
                 FROM roles WHERE id=?",
                [path],
                role_from_row,
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound(format!("Role {path}")))
    }

    pub fn update_role(&mut self, path: &str, input: CreateRole) -> Result<Role, StoreError> {
        validate_name("Role name", &input.name)?;
        validate_rules(&input.rules)?;
        if input.path != path {
            return Err(StoreError::Invalid("Role path is immutable".into()));
        }
        let now = Utc::now();
        let changed = self.connection.execute(
            "UPDATE roles SET name=?,description=?,rules_json=?,updated_at=? WHERE id=? AND managed_by='user'",
            params![input.name, input.description, serde_json::to_string(&input.rules)?, stamp(now), path],
        )?;
        if changed != 1 {
            return Err(StoreError::Conflict(
                "System Role cannot be modified".into(),
            ));
        }
        self.connection.query_row(
            "SELECT id,name,description,rules_json,managed_by,created_at,updated_at FROM roles WHERE id=?",
            [path], role_from_row,
        ).map_err(StoreError::from)
    }

    pub fn delete_role(&mut self, path: &str) -> Result<(), StoreError> {
        let changed = self
            .connection
            .execute("DELETE FROM roles WHERE id=? AND managed_by='user'", [path])
            .map_err(|error| constraint(error, "Role is still bound"))?;
        if changed != 1 {
            return Err(StoreError::Conflict("System Role cannot be deleted".into()));
        }
        Ok(())
    }

    pub fn create_role_binding(
        &mut self,
        input: CreateRoleBinding,
    ) -> Result<RoleBinding, StoreError> {
        validate_name("RoleBinding name", &input.name)?;
        validate_object_path("Referenced Role path", &input.role_path)?;
        if input.subjects.is_empty() {
            return Err(StoreError::Invalid("RoleBinding requires a subject".into()));
        }
        validate_object_path("RoleBinding path", &input.path)?;
        let path = input.path.clone();
        let now = Utc::now();
        let tx = self.connection.transaction()?;
        tx.execute(
            "INSERT INTO role_bindings(id,name,managed_by,created_at) VALUES (?,?,'user',?)",
            params![path, input.name, stamp(now)],
        )
        .map_err(|error| constraint(error, "Role or RoleBinding is invalid"))?;
        let role_link_path = format!("{path}/links/role");
        insert_protected_link_for_role(
            &tx,
            RelationRole::RoleBindingRole,
            &role_link_path,
            ObjectRef {
                kind: ObjectKind::RoleBinding,
                path: path.clone(),
            },
            ObjectRef {
                kind: ObjectKind::Role,
                path: input.role_path.clone(),
            },
            now,
        )?;
        tx.execute(
            "INSERT INTO role_binding_role_index(role_binding_path,role_path,link_path)
             VALUES (?,?,?)",
            params![path, input.role_path, role_link_path],
        )?;
        for (index, subject) in input.subjects.iter().enumerate() {
            ensure_subject_exists(&tx, subject)?;
            let subject_link_path = format!("{path}/links/subjects/{index}");
            insert_protected_link_for_role(
                &tx,
                RelationRole::RoleBindingSubject,
                &subject_link_path,
                ObjectRef {
                    kind: ObjectKind::RoleBinding,
                    path: path.clone(),
                },
                ObjectRef {
                    kind: match subject.kind {
                        SubjectKind::User => ObjectKind::User,
                        SubjectKind::ServiceAccount => ObjectKind::ServiceAccount,
                    },
                    path: subject.path.clone(),
                },
                now,
            )?;
            tx.execute(
                "INSERT INTO role_binding_subjects(
                    role_binding_path,subject_kind,subject_path,link_path
                 ) VALUES (?,?,?,?)",
                params![path, subject.kind.as_str(), subject.path, subject_link_path],
            )?;
        }
        tx.commit()?;
        Ok(RoleBinding {
            path,
            name: input.name,
            role_path: input.role_path,
            subjects: input.subjects,
            managed_by: "user".into(),
            created_at: now,
        })
    }

    pub fn list_role_bindings(&self) -> Result<Vec<RoleBinding>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT rb.id,rb.name,i.role_path,rb.managed_by,rb.created_at
             FROM role_bindings rb JOIN role_binding_role_index i ON i.role_binding_path=rb.id
             ORDER BY rb.name",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                time_from_row(row, 4)?,
            ))
        })?;
        let base = rows.collect::<Result<Vec<_>, _>>()?;
        let mut bindings = Vec::new();
        for (path, name, role_path, managed_by, created_at) in base {
            let mut subjects_statement = self.connection.prepare(
                "SELECT subject_kind,subject_path FROM role_binding_subjects WHERE role_binding_path=? ORDER BY subject_kind,subject_path",
            )?;
            let subjects = subjects_statement
                .query_map([path.as_str()], |row| {
                    let kind: String = row.get(0)?;
                    let kind = match kind.as_str() {
                        "user" => SubjectKind::User,
                        "service_account" => SubjectKind::ServiceAccount,
                        other => return Err(from_sql(0, format!("invalid subject kind {other}"))),
                    };
                    Ok(Subject {
                        kind,
                        path: row.get(1)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            bindings.push(RoleBinding {
                path,
                name,
                role_path,
                subjects,
                managed_by,
                created_at,
            });
        }
        Ok(bindings)
    }

    pub fn delete_role_binding(&mut self, path: &str) -> Result<(), StoreError> {
        let tx = self.connection.transaction()?;
        let managed_by: Option<String> = tx
            .query_row(
                "SELECT managed_by FROM role_bindings WHERE id=?",
                [path],
                |row| row.get(0),
            )
            .optional()?;
        if managed_by.as_deref() != Some("user") {
            return Err(StoreError::Conflict(
                "System RoleBinding cannot be deleted".into(),
            ));
        }
        let link_paths = {
            let mut statement = tx.prepare(
                "SELECT link_path FROM role_binding_subjects WHERE role_binding_path=?
                 UNION ALL
                 SELECT link_path FROM role_binding_role_index WHERE role_binding_path=?",
            )?;
            let values = statement
                .query_map(params![path, path], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            values
        };
        tx.execute(
            "DELETE FROM role_binding_subjects WHERE role_binding_path=?",
            [path],
        )?;
        tx.execute(
            "DELETE FROM role_binding_role_index WHERE role_binding_path=?",
            [path],
        )?;
        for link_path in link_paths {
            tx.execute("DELETE FROM links WHERE id=?", [link_path])?;
        }
        tx.execute("DELETE FROM role_bindings WHERE id=?", [path])?;
        tx.commit()?;
        Ok(())
    }
}

fn apply_mutations(tx: &Transaction<'_>, mutations: &[Mutation]) -> Result<(), StoreError> {
    for mutation in mutations {
        match mutation {
            Mutation::CreateResource { resource } => {
                validate_name("Resource name", &resource.name)?;
                validate_object_path("Mutation Resource path", &resource.path)?;
                validate_object_path("Manifest path", &resource.manifest)?;
                let manifest_path = resource.manifest.clone();
                let (schema, states_json, default_state, initial_state): (
                    String,
                    String,
                    String,
                    String,
                ) = tx
                    .query_row(
                        "SELECT resource_schema_json,states_json,default_state,initial_state
                         FROM manifests WHERE id=?",
                        [&manifest_path],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                    )
                    .optional()?
                    .ok_or_else(|| StoreError::NotFound(format!("Manifest {manifest_path}")))?;
                let states: Vec<String> = serde_json::from_str(&states_json)?;
                let spec =
                    resource_document(resource.spec.clone(), &default_state, &states, false)?;
                let status = resource_document(spec.clone(), &initial_state, &states, true)?;
                validate_json_schema(
                    "Resource spec",
                    &serde_json::from_str::<Value>(&schema)?,
                    &spec,
                )?;
                validate_json_schema(
                    "Resource status",
                    &serde_json::from_str::<Value>(&schema)?,
                    &status,
                )?;
                let now = Utc::now();
                tx.execute(
                    "INSERT INTO resources(id,name,spec_json,status_json,revision,created_at,updated_at)
                     VALUES (?,?,?,?,0,?,?)",
                    params![
                        resource.path.to_string(),
                        resource.name,
                        serde_json::to_string(&spec)?,
                        serde_json::to_string(&status)?,
                        stamp(now),
                        stamp(now)
                    ],
                )
                .map_err(|error| constraint(error, "Mutation Resource already exists"))?;
                for link in &resource.links {
                    insert_link(tx, link, false, now)?;
                }
                let membership_path = format!("{}/links/manifest", resource.path);
                insert_protected_link_for_role(
                    tx,
                    RelationRole::ResourceManifest,
                    &membership_path,
                    ObjectRef {
                        kind: ObjectKind::Resource,
                        path: resource.path.clone(),
                    },
                    ObjectRef {
                        kind: ObjectKind::Manifest,
                        path: manifest_path.clone(),
                    },
                    now,
                )?;
                tx.execute(
                    "INSERT INTO resource_manifest_index(resource_path,manifest_path,link_path)
                     VALUES (?,?,?)",
                    params![resource.path, manifest_path, membership_path],
                )?;
                let created = tx.query_row(
                    RESOURCE_SELECT_BY_ID,
                    [resource.path.to_string()],
                    resource_from_row,
                )?;
                append_lifecycle_event(
                    tx,
                    EventType::Created,
                    ObjectKind::Resource,
                    &resource.path,
                    Some(0),
                    &created,
                    now,
                )?;
                enqueue_resource_if_drifted(tx, &created, "created", now)?;
            }
            Mutation::UpdateResource {
                resource_path,
                expected_revision,
                spec,
            } => {
                let (schema, states_json): (String, String) = tx
                    .query_row(
                        "SELECT m.resource_schema_json,m.states_json FROM resource_manifest_index i
                         JOIN manifests m ON m.id=i.manifest_path WHERE i.resource_path=?",
                        [resource_path.to_string()],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .optional()?
                    .ok_or_else(|| StoreError::NotFound(format!("Resource {resource_path}")))?;
                let states: Vec<String> = serde_json::from_str(&states_json)?;
                validate_resource_state(spec, &states)?;
                validate_json_schema(
                    "Resource spec",
                    &serde_json::from_str::<Value>(&schema)?,
                    spec,
                )?;
                if tx.execute(
                    "UPDATE resources SET spec_json=?,revision=revision+1,updated_at=? WHERE id=? AND revision=?",
                    params![
                        serde_json::to_string(spec)?,
                        stamp(Utc::now()),
                        resource_path.to_string(),
                        expected_revision
                    ],
                )? != 1
                {
                    return Err(StoreError::Conflict(format!(
                        "Resource {resource_path} revision is stale"
                    )));
                }
                let updated = tx.query_row(
                    RESOURCE_SELECT_BY_ID,
                    [resource_path.to_string()],
                    resource_from_row,
                )?;
                append_lifecycle_event(
                    tx,
                    EventType::Updated,
                    ObjectKind::Resource,
                    resource_path,
                    Some(updated.revision),
                    &updated,
                    updated.updated_at,
                )?;
            }
            Mutation::CreateLink { link } => {
                validate_object_path("Mutation Link path", &link.path)?;
                let now = Utc::now();
                insert_link(tx, link, false, now)?;
                let created =
                    tx.query_row(LINK_SELECT_BY_ID, [link.path.to_string()], link_from_row)?;
                append_lifecycle_event(
                    tx,
                    EventType::Created,
                    ObjectKind::Link,
                    &link.path,
                    None,
                    &created,
                    now,
                )?;
                enqueue_link_if_drifted(tx, &created, "created", now)?;
            }
            Mutation::DeleteResource {
                resource_path,
                expected_revision,
            } => {
                let mut resource = tx
                    .query_row(RESOURCE_SELECT_BY_ID, [resource_path], resource_from_row)
                    .optional()?
                    .ok_or_else(|| StoreError::NotFound(format!("Resource {resource_path}")))?;
                if resource.revision != *expected_revision {
                    return Err(StoreError::Conflict(format!(
                        "Resource {resource_path} revision is stale"
                    )));
                }
                set_document_state(&mut resource.spec, kas_core::STATE_DELETED)?;
                let now = Utc::now();
                tx.execute(
                    "UPDATE resources SET spec_json=?,revision=revision+1,updated_at=? WHERE id=?",
                    params![
                        serde_json::to_string(&resource.spec)?,
                        stamp(now),
                        resource_path
                    ],
                )?;
                resource =
                    tx.query_row(RESOURCE_SELECT_BY_ID, [resource_path], resource_from_row)?;
                append_lifecycle_event(
                    tx,
                    EventType::Updated,
                    ObjectKind::Resource,
                    resource_path,
                    Some(resource.revision),
                    &resource,
                    now,
                )?;
                enqueue_resource_if_drifted(tx, &resource, "delete_requested", now)?;
            }
            Mutation::UpdateLink {
                link_path,
                expected_revision,
                source,
                target,
                status,
            } => {
                if source.is_none() && target.is_none() {
                    return Err(StoreError::Invalid(
                        "Link source and target cannot both be null".into(),
                    ));
                }
                for endpoint in source.iter().chain(target.iter()) {
                    ensure_object_exists(tx, endpoint)?;
                }
                validate_link_state(status)?;
                let relation = tx
                    .query_row(
                        "SELECT r.id,r.name,r.role,r.inverse_name,r.sources_json,r.targets_json,
                         r.relation_type,r.ensure,r.on_source_delete,r.metadata_schema_json
                         FROM links l JOIN relations r ON r.id=l.relation_path WHERE l.id=?",
                        [link_path],
                        relation_from_row,
                    )
                    .optional()?
                    .ok_or_else(|| StoreError::NotFound(format!("Link {link_path}")))?;
                if (source.is_none() || target.is_none())
                    && !(relation.ensure && relation.relation_type == RelationType::OneToOne)
                {
                    return Err(StoreError::Invalid(
                        "A partial Link is only valid for an ensured one-to-one Relation".into(),
                    ));
                }
                let source_matches = match source {
                    Some(source) => selectors_match(tx, &relation.sources, source, 0)?,
                    None => true,
                };
                let target_matches = match target {
                    Some(target) => selectors_match(tx, &relation.targets, target, 0)?,
                    None => true,
                };
                if !source_matches || !target_matches {
                    return Err(StoreError::Invalid(format!(
                        "Link {link_path} endpoints do not match Relation {}",
                        relation.path
                    )));
                }
                let now = Utc::now();
                if tx.execute(
                    "UPDATE links SET source_kind=?,source_path=?,target_kind=?,target_path=?,
                     status_json=?,revision=revision+1,updated_at=? WHERE id=? AND revision=?",
                    params![
                        source.as_ref().map(|value| object_kind(&value.kind)),
                        source.as_ref().map(|value| value.path.as_str()),
                        target.as_ref().map(|value| object_kind(&value.kind)),
                        target.as_ref().map(|value| value.path.as_str()),
                        serde_json::to_string(status)?,
                        stamp(now),
                        link_path,
                        expected_revision,
                    ],
                )? != 1
                {
                    return Err(StoreError::Conflict(format!(
                        "Link {link_path} revision is stale"
                    )));
                }
                let updated = tx.query_row(LINK_SELECT_BY_ID, [link_path], link_from_row)?;
                append_lifecycle_event(
                    tx,
                    EventType::Updated,
                    ObjectKind::Link,
                    link_path,
                    Some(updated.revision),
                    &updated,
                    now,
                )?;
                enqueue_link_if_drifted(tx, &updated, "status_updated", now)?;
            }
            Mutation::DeleteLink { link_path } => {
                let link = tx
                    .query_row(LINK_SELECT_BY_ID, [link_path], link_from_row)
                    .optional()?
                    .ok_or_else(|| StoreError::NotFound(format!("Link {link_path}")))?;
                let protected: Option<bool> = tx
                    .query_row(
                        "SELECT protected FROM links WHERE id=?",
                        [link_path],
                        |row| row.get(0),
                    )
                    .optional()?;
                match protected {
                    None => return Err(StoreError::NotFound(format!("Link {link_path}"))),
                    Some(true) => {
                        return Err(StoreError::Invalid(format!(
                            "System Link {link_path} cannot be deleted independently"
                        )))
                    }
                    Some(false) => {}
                }
                tx.execute(
                    "DELETE FROM reconcile_queue WHERE object_kind='link' AND object_path=?",
                    [link_path],
                )?;
                tx.execute("DELETE FROM links WHERE id=?", [link_path])?;
                append_lifecycle_event(
                    tx,
                    EventType::Deleted,
                    ObjectKind::Link,
                    link_path,
                    Some(link.revision),
                    &link,
                    Utc::now(),
                )?;
            }
            Mutation::CreateServiceAccount { path, name } => {
                validate_object_path("ServiceAccount path", path)?;
                validate_name("ServiceAccount name", name)?;
                tx.execute(
                    "INSERT INTO service_accounts(id,name,managed_by,created_at) VALUES (?,?,?,?)",
                    params![path, name, "driver", stamp(Utc::now())],
                )
                .map_err(|error| constraint(error, "ServiceAccount already exists"))?;
            }
            Mutation::DeleteServiceAccount { path } => {
                if tx.execute(
                    "DELETE FROM service_accounts WHERE id=? AND managed_by='driver'",
                    [path],
                )? != 1
                {
                    return Err(StoreError::Conflict(format!(
                        "ServiceAccount {path} does not exist or is not Driver-managed"
                    )));
                }
            }
            Mutation::CreateRoleBinding {
                path,
                name,
                role_path,
                subjects,
            } => {
                insert_driver_role_binding_in_tx(tx, path, name, role_path, subjects, Utc::now())?;
            }
            Mutation::DeleteRoleBinding { path } => {
                delete_driver_role_binding_in_tx(tx, path)?;
            }
            Mutation::UpdateResourceStatus { .. } | Mutation::CompleteRun { .. } => {
                return Err(StoreError::Invalid(
                    "Lifecycle operations must be applied through a Driver delivery".into(),
                ));
            }
        }
    }
    reconcile_ensures_in_tx(tx, Utc::now())?;
    Ok(())
}

fn insert_driver_role_binding_in_tx(
    tx: &Transaction<'_>,
    path: &str,
    name: &str,
    role_path: &str,
    subjects: &[RbacSubjectDefinition],
    now: DateTime<Utc>,
) -> Result<(), StoreError> {
    validate_name("RoleBinding name", name)?;
    validate_object_path("RoleBinding path", path)?;
    validate_object_path("Referenced Role path", role_path)?;
    if subjects.is_empty() {
        return Err(StoreError::Invalid("RoleBinding requires a subject".into()));
    }
    ensure_object_exists(
        tx,
        &ObjectRef {
            kind: ObjectKind::Role,
            path: role_path.to_owned(),
        },
    )?;
    tx.execute(
        "INSERT INTO role_bindings(id,name,managed_by,created_at) VALUES (?,?,'driver',?)",
        params![path, name, stamp(now)],
    )
    .map_err(|error| constraint(error, "Role or RoleBinding is invalid"))?;

    let role_link_path = format!("{path}/links/role");
    insert_protected_link_for_role(
        tx,
        RelationRole::RoleBindingRole,
        &role_link_path,
        ObjectRef {
            kind: ObjectKind::RoleBinding,
            path: path.to_owned(),
        },
        ObjectRef {
            kind: ObjectKind::Role,
            path: role_path.to_owned(),
        },
        now,
    )?;
    tx.execute(
        "INSERT INTO role_binding_role_index(role_binding_path,role_path,link_path)
         VALUES (?,?,?)",
        params![path, role_path, role_link_path],
    )?;

    for (index, subject) in subjects.iter().enumerate() {
        let subject = Subject {
            kind: match subject.kind {
                RbacSubjectKind::User => SubjectKind::User,
                RbacSubjectKind::ServiceAccount => SubjectKind::ServiceAccount,
            },
            path: subject.path.clone(),
        };
        ensure_subject_exists(tx, &subject)?;
        let subject_link_path = format!("{path}/links/subjects/{index}");
        insert_protected_link_for_role(
            tx,
            RelationRole::RoleBindingSubject,
            &subject_link_path,
            ObjectRef {
                kind: ObjectKind::RoleBinding,
                path: path.to_owned(),
            },
            ObjectRef {
                kind: match subject.kind {
                    SubjectKind::User => ObjectKind::User,
                    SubjectKind::ServiceAccount => ObjectKind::ServiceAccount,
                },
                path: subject.path.clone(),
            },
            now,
        )?;
        tx.execute(
            "INSERT INTO role_binding_subjects(
                role_binding_path,subject_kind,subject_path,link_path
             ) VALUES (?,?,?,?)",
            params![path, subject.kind.as_str(), subject.path, subject_link_path],
        )?;
    }
    Ok(())
}

fn delete_driver_role_binding_in_tx(tx: &Transaction<'_>, path: &str) -> Result<(), StoreError> {
    let managed_by: Option<String> = tx
        .query_row(
            "SELECT managed_by FROM role_bindings WHERE id=?",
            [path],
            |row| row.get(0),
        )
        .optional()?;
    if managed_by.as_deref() != Some("driver") {
        return Err(StoreError::Conflict(format!(
            "RoleBinding {path} does not exist or is not Driver-managed"
        )));
    }
    delete_role_binding_rows_in_tx(tx, path)
}

fn delete_role_binding_rows_in_tx(tx: &Transaction<'_>, path: &str) -> Result<(), StoreError> {
    let link_paths = {
        let mut statement = tx.prepare(
            "SELECT link_path FROM role_binding_subjects WHERE role_binding_path=?
             UNION ALL
             SELECT link_path FROM role_binding_role_index WHERE role_binding_path=?",
        )?;
        let values = statement
            .query_map(params![path, path], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        values
    };
    tx.execute(
        "DELETE FROM role_binding_subjects WHERE role_binding_path=?",
        [path],
    )?;
    tx.execute(
        "DELETE FROM role_binding_role_index WHERE role_binding_path=?",
        [path],
    )?;
    for link_path in link_paths {
        tx.execute("DELETE FROM links WHERE id=?", [link_path])?;
    }
    tx.execute("DELETE FROM role_bindings WHERE id=?", [path])?;
    Ok(())
}

fn complete_delivery_in_tx(
    tx: &Transaction<'_>,
    id: Uuid,
    driver_path: &str,
    generation: u64,
) -> Result<(), StoreError> {
    let changed = tx.execute(
        "UPDATE driver_deliveries SET status='completed',completed_at=?
         WHERE id=? AND driver_path=? AND generation=? AND status!='completed'",
        params![
            stamp(Utc::now()),
            id.to_string(),
            driver_path.to_string(),
            generation
        ],
    )?;
    if changed == 1 {
        return Ok(());
    }
    let existing: Option<(String, u64, String)> = tx
        .query_row(
            "SELECT driver_path,generation,status FROM driver_deliveries WHERE id=?",
            [id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    if matches!(
        existing,
        Some((ref existing_driver, existing_generation, ref status))
            if existing_driver == &driver_path.to_string()
                && existing_generation == generation
                && status == "completed"
    ) {
        return Ok(());
    }
    Err(StoreError::Conflict("Delivery is stale".into()))
}

fn append_lifecycle_event(
    tx: &Transaction<'_>,
    event_type: EventType,
    object_kind_value: ObjectKind,
    object_path: &str,
    revision: Option<u64>,
    value: &impl Serialize,
    now: DateTime<Utc>,
) -> Result<(), StoreError> {
    tx.execute(
        "INSERT INTO events(event_type,object_kind,object_path,revision,value_json,created_at)
         VALUES (?,?,?,?,?,?)",
        params![
            event_type_str(event_type),
            object_kind(&object_kind_value),
            object_path.to_string(),
            revision,
            serde_json::to_string(value)?,
            stamp(now)
        ],
    )?;
    Ok(())
}

const DRIVER_SELECT_BY_ID: &str = "SELECT id,desired_state,state,generation,process_id,metadata_json,started_at,heartbeat_at,stopped_at,error,created_at,updated_at FROM drivers WHERE id=?";
const DRIVER_SELECT_BY_MANIFEST: &str = "SELECT d.id,d.desired_state,d.state,d.generation,d.process_id,d.metadata_json,d.started_at,d.heartbeat_at,d.stopped_at,d.error,d.created_at,d.updated_at FROM drivers d JOIN driver_manifest_index i ON i.driver_path=d.id WHERE i.manifest_path=?";
const RESOURCE_SELECT_BY_ID: &str =
    "SELECT id,name,spec_json,status_json,revision,created_at,updated_at FROM resources WHERE id=?";
const RUN_SELECT_BY_ID: &str = "SELECT id,request_id,driver_generation,input_json,status,output_json,error,created_at,started_at,finished_at FROM runs WHERE id=?";
const RUN_SELECT_BY_REQUEST: &str = "SELECT id,request_id,driver_generation,input_json,status,output_json,error,created_at,started_at,finished_at FROM runs WHERE request_id=?";
const LINK_SELECT_BY_ID: &str =
    "SELECT id,source_kind,source_path,relation_path,target_kind,target_path,
spec_json,status_json,metadata_json,revision,created_at,updated_at FROM links WHERE id=?";
const DELIVERY_SELECT_BY_ID: &str = "SELECT id,driver_path,generation,work_json,status,created_at,acked_at,completed_at FROM driver_deliveries WHERE id=?";

fn action_from_row(row: &Row<'_>) -> rusqlite::Result<Action> {
    Ok(Action {
        path: row.get(0)?,
        name: row.get(1)?,
        description: row.get(2)?,
        input_schema: json_from_row(row, 3)?,
        output_schema: json_from_row(row, 4)?,
    })
}

fn relation_from_row(row: &Row<'_>) -> rusqlite::Result<Relation> {
    let role: Option<String> = row.get(2)?;
    Ok(Relation {
        path: row.get(0)?,
        name: row.get(1)?,
        role: match role.as_deref() {
            None => None,
            Some(value) => Some(relation_role_from_str(value, 2)?),
        },
        inverse_name: row.get(3)?,
        sources: json_from_row(row, 4)?,
        targets: json_from_row(row, 5)?,
        relation_type: relation_type_from_str(&row.get::<_, String>(6)?, 6)?,
        ensure: row.get(7)?,
        on_source_delete: on_source_delete_from_str(&row.get::<_, String>(8)?, 8)?,
        metadata_schema: json_from_row(row, 9)?,
    })
}

fn link_from_row(row: &Row<'_>) -> rusqlite::Result<Link> {
    Ok(Link {
        path: row.get(0)?,
        source: optional_object_ref(row, 1, 2)?,
        relation_path: row.get(3)?,
        target: optional_object_ref(row, 4, 5)?,
        spec: json_from_row(row, 6)?,
        status: json_from_row(row, 7)?,
        metadata: json_from_row(row, 8)?,
        revision: row.get(9)?,
        created_at: time_from_row(row, 10)?,
        updated_at: time_from_row(row, 11)?,
    })
}

fn optional_object_ref(
    row: &Row<'_>,
    kind_index: usize,
    path_index: usize,
) -> rusqlite::Result<Option<ObjectRef>> {
    let kind: Option<String> = row.get(kind_index)?;
    let path: Option<String> = row.get(path_index)?;
    match (kind, path) {
        (Some(kind), Some(path)) => Ok(Some(ObjectRef {
            kind: object_kind_from_str(&kind, kind_index)?,
            path,
        })),
        (None, None) => Ok(None),
        _ => Err(from_sql(kind_index, "incomplete Link endpoint".into())),
    }
}

fn event_from_row(row: &Row<'_>) -> rusqlite::Result<Event> {
    Ok(Event {
        sequence: row.get(0)?,
        event_type: event_type_from_str(&row.get::<_, String>(1)?, 1)?,
        object_kind: object_kind_from_str(&row.get::<_, String>(2)?, 2)?,
        object_path: row.get(3)?,
        revision: row.get(4)?,
        value: json_from_row(row, 5)?,
        created_at: time_from_row(row, 6)?,
    })
}

fn delivery_from_row(row: &Row<'_>) -> rusqlite::Result<DriverDelivery> {
    Ok(DriverDelivery {
        id: uuid_from_row(row, 0)?,
        driver_path: row.get(1)?,
        generation: row.get(2)?,
        work: json_from_row(row, 3)?,
        status: match row.get::<_, String>(4)?.as_str() {
            "pending" => DeliveryStatus::Pending,
            "acked" => DeliveryStatus::Acked,
            "completed" => DeliveryStatus::Completed,
            value => return Err(from_sql(4, format!("invalid delivery status {value}"))),
        },
        created_at: time_from_row(row, 5)?,
        acked_at: optional_time_from_row(row, 6)?,
        completed_at: optional_time_from_row(row, 7)?,
    })
}

fn resource_from_row(row: &Row<'_>) -> rusqlite::Result<Resource> {
    Ok(Resource {
        path: row.get(0)?,
        name: row.get(1)?,
        spec: json_from_row(row, 2)?,
        status: json_from_row(row, 3)?,
        revision: row.get(4)?,
        created_at: time_from_row(row, 5)?,
        updated_at: time_from_row(row, 6)?,
    })
}

fn user_from_row(row: &Row<'_>) -> rusqlite::Result<User> {
    Ok(User {
        path: row.get(0)?,
        name: row.get(1)?,
        disabled: row.get(2)?,
        created_at: time_from_row(row, 3)?,
    })
}

fn role_from_row(row: &Row<'_>) -> rusqlite::Result<Role> {
    Ok(Role {
        path: row.get(0)?,
        name: row.get(1)?,
        description: row.get(2)?,
        rules: json_from_row(row, 3)?,
        managed_by: row.get(4)?,
        created_at: time_from_row(row, 5)?,
        updated_at: time_from_row(row, 6)?,
    })
}

fn parse_subject_kind(value: &str) -> Result<SubjectKind, StoreError> {
    match value {
        "user" => Ok(SubjectKind::User),
        "service_account" => Ok(SubjectKind::ServiceAccount),
        other => Err(StoreError::Invalid(format!("Unknown subject kind {other}"))),
    }
}

fn ensure_subject_exists(tx: &Transaction<'_>, subject: &Subject) -> Result<(), StoreError> {
    validate_object_path("Subject path", &subject.path)?;
    let table = match subject.kind {
        SubjectKind::User => "users",
        SubjectKind::ServiceAccount => "service_accounts",
    };
    let exists: bool = tx.query_row(
        &format!("SELECT EXISTS(SELECT 1 FROM {table} WHERE id=?)"),
        [subject.path.to_string()],
        |row| row.get(0),
    )?;
    if !exists {
        return Err(StoreError::NotFound(format!(
            "{} {}",
            subject.kind.as_str(),
            subject.path
        )));
    }
    Ok(())
}

fn driver_from_row(row: &Row<'_>) -> rusqlite::Result<Driver> {
    Ok(Driver {
        path: row.get(0)?,
        desired_state: match row.get::<_, String>(1)?.as_str() {
            "stopped" => DriverDesiredState::Stopped,
            "running" => DriverDesiredState::Running,
            state => return Err(from_sql(1, format!("invalid desired Driver state {state}"))),
        },
        state: match row.get::<_, String>(2)?.as_str() {
            "stopped" => DriverState::Stopped,
            "starting" => DriverState::Starting,
            "ready" => DriverState::Ready,
            "stopping" => DriverState::Stopping,
            "failed" => DriverState::Failed,
            state => return Err(from_sql(2, format!("invalid Driver state {state}"))),
        },
        generation: row.get(3)?,
        process_id: row.get(4)?,
        metadata: json_from_row(row, 5)?,
        started_at: optional_time_from_row(row, 6)?,
        heartbeat_at: optional_time_from_row(row, 7)?,
        stopped_at: optional_time_from_row(row, 8)?,
        error: row.get(9)?,
        created_at: time_from_row(row, 10)?,
        updated_at: time_from_row(row, 11)?,
    })
}

fn run_from_row(row: &Row<'_>) -> rusqlite::Result<Run> {
    Ok(Run {
        path: row.get(0)?,
        request_id: uuid_from_row(row, 1)?,
        driver_generation: row.get(2)?,
        input: json_from_row(row, 3)?,
        status: match row.get::<_, String>(4)?.as_str() {
            "queued" => RunStatus::Queued,
            "running" => RunStatus::Running,
            "succeeded" => RunStatus::Succeeded,
            "failed" => RunStatus::Failed,
            "cancelled" => RunStatus::Cancelled,
            status => return Err(from_sql(4, format!("invalid Run status {status}"))),
        },
        output: optional_json_from_row(row, 5)?,
        error: row.get(6)?,
        created_at: time_from_row(row, 7)?,
        started_at: optional_time_from_row(row, 8)?,
        finished_at: optional_time_from_row(row, 9)?,
    })
}

fn validate_manifest_contract(resource_schema: &Value) -> Result<(), StoreError> {
    validate_json_schema_contract("Resource schema", resource_schema)
}

fn validate_json_schema_contract(label: &str, schema: &Value) -> Result<(), StoreError> {
    jsonschema::validator_for(schema)
        .map_err(|error| StoreError::Invalid(format!("{label} is invalid: {error}")))?;
    Ok(())
}

fn validate_json_schema(label: &str, schema: &Value, instance: &Value) -> Result<(), StoreError> {
    let validator = jsonschema::validator_for(schema)
        .map_err(|error| StoreError::Invalid(format!("{label} schema is invalid: {error}")))?;
    validator
        .validate(instance)
        .map_err(|error| StoreError::Invalid(format!("{label} does not match its schema: {error}")))
}

fn event_type_str(event_type: EventType) -> &'static str {
    match event_type {
        EventType::Created => "created",
        EventType::Updated => "updated",
        EventType::Deleted => "deleted",
    }
}

fn event_type_from_str(value: &str, index: usize) -> rusqlite::Result<EventType> {
    match value {
        "created" => Ok(EventType::Created),
        "updated" => Ok(EventType::Updated),
        "deleted" => Ok(EventType::Deleted),
        _ => Err(from_sql(index, format!("invalid Event type {value}"))),
    }
}

fn object_kind(kind: &ObjectKind) -> &'static str {
    match kind {
        ObjectKind::Manifest => "manifest",
        ObjectKind::Action => "action",
        ObjectKind::Relation => "relation",
        ObjectKind::Resource => "resource",
        ObjectKind::Driver => "driver",
        ObjectKind::Run => "run",
        ObjectKind::Link => "link",
        ObjectKind::User => "user",
        ObjectKind::ServiceAccount => "service_account",
        ObjectKind::Role => "role",
        ObjectKind::RoleBinding => "role_binding",
        ObjectKind::Credential => "credential",
    }
}

fn relation_role(role: RelationRole) -> &'static str {
    match role {
        RelationRole::ManifestMember => "manifest_member",
        RelationRole::ResourceManifest => "resource_manifest",
        RelationRole::RunResource => "run_resource",
        RelationRole::RunAction => "run_action",
        RelationRole::RunDriver => "run_driver",
        RelationRole::DriverServiceAccount => "driver_service_account",
        RelationRole::RoleBindingRole => "role_binding_role",
        RelationRole::RoleBindingSubject => "role_binding_subject",
    }
}

fn relation_role_from_str(value: &str, index: usize) -> rusqlite::Result<RelationRole> {
    match value {
        "manifest_member" => Ok(RelationRole::ManifestMember),
        "resource_manifest" => Ok(RelationRole::ResourceManifest),
        "run_resource" => Ok(RelationRole::RunResource),
        "run_action" => Ok(RelationRole::RunAction),
        "run_driver" => Ok(RelationRole::RunDriver),
        "driver_service_account" => Ok(RelationRole::DriverServiceAccount),
        "role_binding_role" => Ok(RelationRole::RoleBindingRole),
        "role_binding_subject" => Ok(RelationRole::RoleBindingSubject),
        other => Err(from_sql(index, format!("invalid Relation role {other}"))),
    }
}

fn relation_type(value: RelationType) -> &'static str {
    match value {
        RelationType::OneToOne => "one_to_one",
        RelationType::OneToMany => "one_to_many",
        RelationType::ManyToOne => "many_to_one",
        RelationType::ManyToMany => "many_to_many",
    }
}

fn relation_type_from_str(value: &str, index: usize) -> rusqlite::Result<RelationType> {
    match value {
        "one_to_one" => Ok(RelationType::OneToOne),
        "one_to_many" => Ok(RelationType::OneToMany),
        "many_to_one" => Ok(RelationType::ManyToOne),
        "many_to_many" => Ok(RelationType::ManyToMany),
        other => Err(from_sql(index, format!("invalid Relation type {other}"))),
    }
}

fn on_source_delete(value: OnSourceDelete) -> &'static str {
    match value {
        OnSourceDelete::Unlink => "unlink",
        OnSourceDelete::Cascade => "cascade",
    }
}

fn on_source_delete_from_str(value: &str, index: usize) -> rusqlite::Result<OnSourceDelete> {
    match value {
        "unlink" => Ok(OnSourceDelete::Unlink),
        "cascade" => Ok(OnSourceDelete::Cascade),
        other => Err(from_sql(
            index,
            format!("invalid source deletion policy {other}"),
        )),
    }
}

fn system_role(role: SystemRole) -> &'static str {
    match role {
        SystemRole::Admin => "admin",
        SystemRole::Editor => "editor",
        SystemRole::Viewer => "viewer",
    }
}

fn system_role_from_str(value: &str, index: usize) -> rusqlite::Result<SystemRole> {
    match value {
        "admin" => Ok(SystemRole::Admin),
        "editor" => Ok(SystemRole::Editor),
        "viewer" => Ok(SystemRole::Viewer),
        other => Err(from_sql(index, format!("invalid system Role {other}"))),
    }
}

fn rbac_subject_kind(kind: RbacSubjectKind) -> &'static str {
    match kind {
        RbacSubjectKind::User => "user",
        RbacSubjectKind::ServiceAccount => "service_account",
    }
}

fn object_kind_from_str(value: &str, index: usize) -> rusqlite::Result<ObjectKind> {
    match value {
        "manifest" => Ok(ObjectKind::Manifest),
        "action" => Ok(ObjectKind::Action),
        "relation" => Ok(ObjectKind::Relation),
        "resource" => Ok(ObjectKind::Resource),
        "driver" => Ok(ObjectKind::Driver),
        "run" => Ok(ObjectKind::Run),
        "link" => Ok(ObjectKind::Link),
        "user" => Ok(ObjectKind::User),
        "service_account" => Ok(ObjectKind::ServiceAccount),
        "role" => Ok(ObjectKind::Role),
        "role_binding" => Ok(ObjectKind::RoleBinding),
        "credential" => Ok(ObjectKind::Credential),
        _ => Err(from_sql(index, format!("invalid Object kind {value}"))),
    }
}

fn ensure_object_exists(connection: &Connection, object: &ObjectRef) -> Result<(), StoreError> {
    validate_object_path("Object reference path", &object.path)?;
    let table = match object.kind {
        ObjectKind::Manifest => "manifests",
        ObjectKind::Action => "actions",
        ObjectKind::Relation => "relations",
        ObjectKind::Resource => "resources",
        ObjectKind::Driver => "drivers",
        ObjectKind::Run => "runs",
        ObjectKind::Link => "links",
        ObjectKind::User => "users",
        ObjectKind::ServiceAccount => "service_accounts",
        ObjectKind::Role => "roles",
        ObjectKind::RoleBinding => "role_bindings",
        ObjectKind::Credential => "credentials",
    };
    let exists: bool = connection.query_row(
        &format!("SELECT EXISTS(SELECT 1 FROM {table} WHERE id=?)"),
        [object.path.to_string()],
        |row| row.get(0),
    )?;
    if !exists {
        return Err(StoreError::NotFound(format!(
            "{} {}",
            object_kind(&object.kind),
            object.path
        )));
    }
    Ok(())
}

fn relation_path_by_role(
    connection: &Connection,
    role: RelationRole,
) -> Result<String, StoreError> {
    connection
        .query_row(
            "SELECT id FROM relations WHERE role=?",
            [relation_role(role)],
            |row| row.get(0),
        )
        .optional()?
        .ok_or_else(|| {
            StoreError::Invalid(format!(
                "required built-in Relation role {} is not installed",
                relation_role(role)
            ))
        })
}

fn insert_protected_link_for_role(
    tx: &Transaction<'_>,
    role: RelationRole,
    link_path: &str,
    source: ObjectRef,
    target: ObjectRef,
    now: DateTime<Utc>,
) -> Result<(), StoreError> {
    let relation_path = relation_path_by_role(tx, role)?;
    insert_link(
        tx,
        &PlannedLink {
            path: link_path.to_owned(),
            source: Some(source),
            relation_path,
            target: Some(target),
            spec: json!({ "state": kas_core::STATE_AVAILABLE }),
            status: json!({ "state": kas_core::STATE_AVAILABLE }),
            metadata: json!({}),
        },
        true,
        now,
    )
}

fn insert_link(
    tx: &Transaction<'_>,
    link: &PlannedLink,
    protected: bool,
    now: DateTime<Utc>,
) -> Result<(), StoreError> {
    validate_object_path("Link path", &link.path)?;
    validate_object_path("Relation path", &link.relation_path)?;
    validate_link_state(&link.spec)?;
    validate_link_state(&link.status)?;
    if link.source.is_none() && link.target.is_none() {
        return Err(StoreError::Invalid(
            "Link source and target cannot both be null".into(),
        ));
    }
    if let Some(source) = &link.source {
        ensure_object_exists(tx, source)?;
    }
    if let Some(target) = &link.target {
        ensure_object_exists(tx, target)?;
    }
    let (sources_json, targets_json, relation_type_value, ensure, metadata_schema_json, system): (
        String,
        String,
        String,
        bool,
        String,
        bool,
    ) = tx
        .query_row(
            "SELECT sources_json,targets_json,relation_type,ensure,metadata_schema_json,protected
             FROM relations WHERE id=?",
            [&link.relation_path],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            },
        )
        .optional()?
        .ok_or_else(|| StoreError::NotFound(format!("Relation {}", link.relation_path)))?;
    if system != protected {
        return Err(StoreError::Invalid(if system {
            format!(
                "System Relation {} can only be used by an atomic platform operation",
                link.relation_path
            )
        } else {
            format!("Business Link {} cannot be marked protected", link.path)
        }));
    }
    let sources: Vec<ObjectSelector> = serde_json::from_str(&sources_json)?;
    let targets: Vec<ObjectSelector> = serde_json::from_str(&targets_json)?;
    let source_matches = match &link.source {
        Some(source) => selectors_match(tx, &sources, source, 0)?,
        None => true,
    };
    let target_matches = match &link.target {
        Some(target) => selectors_match(tx, &targets, target, 0)?,
        None => true,
    };
    if !source_matches || !target_matches {
        return Err(StoreError::Invalid(format!(
            "Link {} endpoints do not match Relation {}",
            link.path, link.relation_path
        )));
    }
    let relation_type =
        relation_type_from_str(&relation_type_value, 2).map_err(StoreError::Database)?;
    if (link.source.is_none() || link.target.is_none())
        && !(ensure && relation_type == RelationType::OneToOne)
    {
        return Err(StoreError::Invalid(
            "A partial Link is only valid for an ensured one-to-one Relation".into(),
        ));
    }
    let metadata_schema: Value = serde_json::from_str(&metadata_schema_json)?;
    validate_json_schema("Link metadata", &metadata_schema, &link.metadata)?;
    tx.execute(
        "INSERT INTO links(id,source_kind,source_path,relation_path,target_kind,target_path,
         spec_json,status_json,metadata_json,revision,protected,created_at,updated_at)
         VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?)",
        params![
            link.path,
            link.source.as_ref().map(|value| object_kind(&value.kind)),
            link.source.as_ref().map(|value| value.path.as_str()),
            link.relation_path,
            link.target.as_ref().map(|value| object_kind(&value.kind)),
            link.target.as_ref().map(|value| value.path.as_str()),
            serde_json::to_string(&link.spec)?,
            serde_json::to_string(&link.status)?,
            serde_json::to_string(&link.metadata)?,
            0,
            protected,
            stamp(now),
            stamp(now)
        ],
    )
    .map_err(|error| constraint(error, "Link already exists"))?;
    Ok(())
}

fn selectors_match(
    connection: &Connection,
    selectors: &[ObjectSelector],
    object: &ObjectRef,
    depth: usize,
) -> Result<bool, StoreError> {
    for selector in selectors {
        if selector_matches(connection, selector, object, depth)? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn selector_matches(
    connection: &Connection,
    selector: &ObjectSelector,
    object: &ObjectRef,
    depth: usize,
) -> Result<bool, StoreError> {
    if depth > 16 {
        return Err(StoreError::Invalid(
            "Relation selector nesting exceeds 16 levels".into(),
        ));
    }
    if selector
        .kind
        .as_ref()
        .is_some_and(|kind| !kind.matches(object.kind))
    {
        return Ok(false);
    }
    if selector
        .path
        .as_ref()
        .is_some_and(|pattern| !kas_auth::path_matches(pattern, &object.path))
    {
        return Ok(false);
    }
    if !selector.any_of.is_empty()
        && !selectors_match(connection, &selector.any_of, object, depth + 1)?
    {
        return Ok(false);
    }
    for link_selector in &selector.links {
        let (object_column, other_kind, other_path) = match link_selector.direction {
            LinkDirection::Source => ("source", "target_kind", "target_path"),
            LinkDirection::Target => ("target", "source_kind", "source_path"),
            LinkDirection::Either => {
                let source_matches = linked_object_matches(
                    connection,
                    "source",
                    "target_kind",
                    "target_path",
                    link_selector,
                    object,
                    depth,
                )?;
                let target_matches = linked_object_matches(
                    connection,
                    "target",
                    "source_kind",
                    "source_path",
                    link_selector,
                    object,
                    depth,
                )?;
                if !source_matches && !target_matches {
                    return Ok(false);
                }
                continue;
            }
        };
        if !linked_object_matches(
            connection,
            object_column,
            other_kind,
            other_path,
            link_selector,
            object,
            depth,
        )? {
            return Ok(false);
        }
    }
    Ok(true)
}

fn linked_object_matches(
    connection: &Connection,
    object_side: &str,
    other_kind_column: &str,
    other_path_column: &str,
    selector: &kas_core::LinkSelector,
    object: &ObjectRef,
    depth: usize,
) -> Result<bool, StoreError> {
    let sql = format!(
        "SELECT {other_kind_column},{other_path_column} FROM links
         WHERE {object_side}_kind=? AND {object_side}_path=? AND relation_path=?"
    );
    let mut statement = connection.prepare(&sql)?;
    let rows = statement.query_map(
        params![
            object_kind(&object.kind),
            object.path,
            selector.relation_path
        ],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
    )?;
    for row in rows {
        let (kind, path) = row?;
        let linked = ObjectRef {
            kind: object_kind_from_str(&kind, 0)?,
            path,
        };
        if selector.object.is_none() {
            return Ok(true);
        }
        if let Some(nested) = selector.object.as_ref() {
            if selector_matches(connection, nested, &linked, depth + 1)? {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn validate_package_digest(digest: &str) -> Result<(), StoreError> {
    let Some(hex) = digest.strip_prefix("sha256:") else {
        return Err(StoreError::Invalid(
            "Package digest must use the sha256:<hex> format".into(),
        ));
    };
    if hex.len() != 64
        || !hex
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(StoreError::Invalid(
            "Package digest must contain 64 lowercase hexadecimal characters".into(),
        ));
    }
    Ok(())
}

fn validate_manifest_member_path(
    manifest_path: &str,
    collection: &str,
    path: &str,
) -> Result<(), StoreError> {
    let prefix = format!("{manifest_path}/{collection}/");
    validate_object_path("Manifest member path", path)?;
    if !path.starts_with(&prefix) {
        return Err(StoreError::Invalid(format!(
            "Manifest member path {path} must be under {manifest_path}/{collection}"
        )));
    }
    Ok(())
}

fn validate_package_entrypoint(entrypoint: &str) -> Result<(), StoreError> {
    let Some(relative) = entrypoint.strip_prefix("./") else {
        return Err(StoreError::Invalid(
            "Driver entrypoint must be package-relative and start with ./".into(),
        ));
    };
    if relative.is_empty()
        || relative
            .split('/')
            .any(|segment| segment.is_empty() || segment == "." || segment == "..")
    {
        return Err(StoreError::Invalid(
            "Driver entrypoint contains an invalid path segment".into(),
        ));
    }
    Ok(())
}

fn validate_relation_definition(relation: &Relation) -> Result<(), StoreError> {
    validate_name("Relation name", &relation.name)?;
    if relation.sources.is_empty() || relation.targets.is_empty() {
        return Err(StoreError::Invalid(
            "Relation sources and targets cannot be empty".into(),
        ));
    }
    if relation.ensure && relation.relation_type != RelationType::OneToOne {
        return Err(StoreError::Invalid(
            "Only a one-to-one Relation can currently declare ensure".into(),
        ));
    }
    for selector in relation.sources.iter().chain(&relation.targets) {
        validate_object_selector(selector, 0)?;
    }
    if relation.metadata_schema.is_null() {
        Ok(())
    } else {
        validate_json_schema_contract("Relation metadata schema", &relation.metadata_schema)
    }
}

fn validate_manifest_states(
    states: &[String],
    default_state: &str,
    initial_state: &str,
) -> Result<(), StoreError> {
    let mut seen = std::collections::HashSet::new();
    for state in states {
        validate_name("Manifest state", state)?;
        if matches!(
            state.as_str(),
            kas_core::STATE_PENDING | kas_core::STATE_AVAILABLE | kas_core::STATE_DELETED
        ) {
            return Err(StoreError::Invalid(format!(
                "Manifest state {state} is reserved by KAS"
            )));
        }
        if !seen.insert(state) {
            return Err(StoreError::Invalid(format!(
                "Manifest state {state} is duplicated"
            )));
        }
    }
    for (label, value) in [
        ("default_state", default_state),
        ("initial_state", initial_state),
    ] {
        if value == kas_core::STATE_DELETED
            || (!matches!(value, kas_core::STATE_PENDING | kas_core::STATE_AVAILABLE)
                && !states.iter().any(|state| state == value))
        {
            return Err(StoreError::Invalid(format!(
                "Manifest {label} {value} is not an allowed creation state"
            )));
        }
    }
    Ok(())
}

fn validate_resource_state(value: &Value, states: &[String]) -> Result<(), StoreError> {
    let state = value
        .as_object()
        .and_then(|object| object.get("state"))
        .and_then(Value::as_str)
        .ok_or_else(|| {
            StoreError::Invalid("Resource spec/status must contain string state".into())
        })?;
    if matches!(
        state,
        kas_core::STATE_PENDING | kas_core::STATE_AVAILABLE | kas_core::STATE_DELETED
    ) || states.iter().any(|candidate| candidate == state)
    {
        Ok(())
    } else {
        Err(StoreError::Invalid(format!(
            "Resource state {state} is not declared by its Manifest"
        )))
    }
}

fn validate_link_state(value: &Value) -> Result<(), StoreError> {
    let state = value
        .as_object()
        .and_then(|object| object.get("state"))
        .and_then(Value::as_str)
        .ok_or_else(|| StoreError::Invalid("Link spec/status must contain string state".into()))?;
    if matches!(
        state,
        kas_core::STATE_PENDING | kas_core::STATE_AVAILABLE | kas_core::STATE_DELETED
    ) {
        Ok(())
    } else {
        Err(StoreError::Invalid(format!(
            "Link state {state} is not a platform state"
        )))
    }
}

fn resource_document(
    mut value: Value,
    state: &str,
    states: &[String],
    replace: bool,
) -> Result<Value, StoreError> {
    let object = value
        .as_object_mut()
        .ok_or_else(|| StoreError::Invalid("Resource spec/status must be an object".into()))?;
    if replace || !object.contains_key("state") {
        object.insert("state".into(), Value::String(state.into()));
    }
    validate_resource_state(&value, states)?;
    Ok(value)
}

fn set_document_state(value: &mut Value, state: &str) -> Result<(), StoreError> {
    value
        .as_object_mut()
        .ok_or_else(|| StoreError::Invalid("state document must be an object".into()))?
        .insert("state".into(), Value::String(state.into()));
    Ok(())
}

fn document_state(value: &Value) -> Option<&str> {
    value
        .as_object()
        .and_then(|object| object.get("state"))
        .and_then(Value::as_str)
}

fn finalize_deleted_resource_in_tx(
    tx: &Transaction<'_>,
    resource: &Resource,
    now: DateTime<Utc>,
) -> Result<bool, StoreError> {
    if document_state(&resource.spec) != Some(kas_core::STATE_DELETED)
        || document_state(&resource.status) != Some(kas_core::STATE_DELETED)
    {
        return Ok(false);
    }

    let cascade_targets = {
        let mut statement = tx.prepare(
            "SELECT l.target_kind,l.target_path FROM links l
             JOIN relations r ON r.id=l.relation_path
             WHERE l.source_kind='resource' AND l.source_path=?
               AND r.on_source_delete='cascade' AND l.target_path IS NOT NULL",
        )?;
        let values = statement
            .query_map([&resource.path], |row| {
                let kind: String = row.get(0)?;
                Ok(ObjectRef {
                    kind: object_kind_from_str(&kind, 0)?,
                    path: row.get(1)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        values
    };
    for target in cascade_targets {
        match target.kind {
            ObjectKind::Resource => {
                let current: Option<(String, u64)> = tx
                    .query_row(
                        "SELECT spec_json,revision FROM resources WHERE id=?",
                        [&target.path],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .optional()?;
                if let Some((raw, _revision)) = current {
                    let mut spec: Value = serde_json::from_str(&raw)?;
                    set_document_state(&mut spec, kas_core::STATE_DELETED)?;
                    tx.execute(
                        "UPDATE resources SET spec_json=?,revision=revision+1,updated_at=? WHERE id=?",
                        params![serde_json::to_string(&spec)?, stamp(now), target.path],
                    )?;
                    let target_resource =
                        tx.query_row(RESOURCE_SELECT_BY_ID, [&target.path], resource_from_row)?;
                    enqueue_resource_if_drifted(tx, &target_resource, "cascade_delete", now)?;
                }
            }
            ObjectKind::ServiceAccount => {
                let role_bindings = {
                    let mut statement = tx.prepare(
                        "SELECT rb.id FROM role_bindings rb
                         JOIN role_binding_subjects s ON s.role_binding_path=rb.id
                         WHERE rb.managed_by='driver'
                           AND s.subject_kind='service_account'
                           AND s.subject_path=?",
                    )?;
                    let values = statement
                        .query_map([&target.path], |row| row.get::<_, String>(0))?
                        .collect::<Result<Vec<_>, _>>()?;
                    values
                };
                for role_binding in role_bindings {
                    delete_role_binding_rows_in_tx(tx, &role_binding)?;
                }
                tx.execute("DELETE FROM credentials WHERE subject_kind='service_account' AND subject_path=?", [&target.path])?;
                tx.execute(
                    "DELETE FROM service_accounts WHERE id=? AND managed_by IN ('user','driver')",
                    [&target.path],
                )?;
            }
            _ => {}
        }
    }

    let run_links = {
        let mut statement = tx.prepare(
            "SELECT run_path,resource_link_path,action_link_path,driver_link_path
             FROM run_relation_index WHERE resource_path=?",
        )?;
        let values = statement
            .query_map([&resource.path], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        values
    };
    for (run_path, resource_link, action_link, driver_link) in run_links {
        tx.execute(
            "DELETE FROM run_relation_index WHERE run_path=?",
            [&run_path],
        )?;
        tx.execute(
            "DELETE FROM links WHERE id IN (?,?,?)",
            params![resource_link, action_link, driver_link],
        )?;
        tx.execute("DELETE FROM runs WHERE id=?", [&run_path])?;
    }

    if let Some(membership_link) = tx
        .query_row(
            "SELECT link_path FROM resource_manifest_index WHERE resource_path=?",
            [&resource.path],
            |row| row.get::<_, String>(0),
        )
        .optional()?
    {
        tx.execute(
            "DELETE FROM resource_manifest_index WHERE resource_path=?",
            [&resource.path],
        )?;
        tx.execute("DELETE FROM links WHERE id=?", [membership_link])?;
    }
    tx.execute(
        "DELETE FROM reconcile_queue WHERE object_kind='resource' AND object_path=?",
        [&resource.path],
    )?;
    tx.execute(
        "DELETE FROM reconcile_queue WHERE object_kind='link' AND object_path IN (
           SELECT id FROM links WHERE
             (source_kind='resource' AND source_path=?) OR
             (target_kind='resource' AND target_path=?)
         )",
        params![resource.path, resource.path],
    )?;
    tx.execute(
        "DELETE FROM links WHERE
           (source_kind='resource' AND source_path=?) OR
           (target_kind='resource' AND target_path=?)",
        params![resource.path, resource.path],
    )?;
    tx.execute("DELETE FROM resources WHERE id=?", [&resource.path])?;
    append_lifecycle_event(
        tx,
        EventType::Deleted,
        ObjectKind::Resource,
        &resource.path,
        Some(resource.revision),
        resource,
        now,
    )?;
    reconcile_ensures_in_tx(tx, now)?;
    Ok(true)
}

fn enqueue_resource_if_drifted(
    tx: &Transaction<'_>,
    resource: &Resource,
    reason: &str,
    now: DateTime<Utc>,
) -> Result<(), StoreError> {
    if resource.spec == resource.status {
        tx.execute(
            "DELETE FROM reconcile_queue WHERE object_kind='resource' AND object_path=?",
            [&resource.path],
        )?;
        return Ok(());
    }
    tx.execute(
        "INSERT INTO reconcile_queue(object_kind,object_path,driver_path,reason,available_at,updated_at)
         SELECT 'resource',?,di.driver_path,?,?,? FROM resource_manifest_index ri
         JOIN driver_manifest_index di ON di.manifest_path=ri.manifest_path
         WHERE ri.resource_path=?
         ON CONFLICT(object_kind,object_path) DO UPDATE SET
           driver_path=excluded.driver_path,reason=excluded.reason,
           available_at=excluded.available_at,updated_at=excluded.updated_at",
        params![
            resource.path,
            reason,
            stamp(now),
            stamp(now),
            resource.path
        ],
    )?;
    Ok(())
}

fn enqueue_link_if_drifted(
    tx: &Transaction<'_>,
    link: &Link,
    reason: &str,
    now: DateTime<Utc>,
) -> Result<(), StoreError> {
    if link.spec == link.status {
        tx.execute(
            "DELETE FROM reconcile_queue WHERE object_kind='link' AND object_path=?",
            [&link.path],
        )?;
        return Ok(());
    }
    tx.execute(
        "INSERT INTO reconcile_queue(object_kind,object_path,driver_path,reason,available_at,updated_at)
         SELECT 'link',?,di.driver_path,?,?,? FROM relations r
         JOIN driver_manifest_index di ON di.manifest_path=r.manifest_path
         WHERE r.id=?
         ON CONFLICT(object_kind,object_path) DO UPDATE SET
           driver_path=excluded.driver_path,reason=excluded.reason,
           available_at=excluded.available_at,updated_at=excluded.updated_at",
        params![link.path, reason, stamp(now), stamp(now), link.relation_path],
    )?;
    Ok(())
}

fn reconcile_ensures_in_tx(tx: &Transaction<'_>, now: DateTime<Utc>) -> Result<(), StoreError> {
    let relations = {
        let mut statement = tx.prepare(
            "SELECT id,name,role,inverse_name,sources_json,targets_json,
             relation_type,ensure,on_source_delete,metadata_schema_json
             FROM relations WHERE ensure=1 ORDER BY id",
        )?;
        let values = statement
            .query_map([], relation_from_row)?
            .collect::<Result<Vec<_>, _>>()?;
        values
    };
    if relations.is_empty() {
        return Ok(());
    }
    let objects = all_object_refs(tx)?;
    for relation in relations {
        let mut source_candidates = Vec::new();
        let mut target_candidates = Vec::new();
        for object in &objects {
            if selectors_match(tx, &relation.sources, object, 0)? {
                source_candidates.push(object.clone());
            }
            if selectors_match(tx, &relation.targets, object, 0)? {
                target_candidates.push(object.clone());
            }
        }

        let mut created_source_partial = false;
        for source in &source_candidates {
            let linked: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM links
                 WHERE relation_path=? AND source_kind=? AND source_path=?)",
                params![relation.path, object_kind(&source.kind), source.path],
                |row| row.get(0),
            )?;
            if !linked {
                insert_ensure_link(tx, &relation, Some(source.clone()), None, now)?;
                created_source_partial = true;
            }
        }
        let has_source_endpoint: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM links WHERE relation_path=? AND source_path IS NOT NULL)",
            [&relation.path],
            |row| row.get(0),
        )?;
        if created_source_partial || has_source_endpoint || !source_candidates.is_empty() {
            continue;
        }
        for target in &target_candidates {
            let linked: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM links
                 WHERE relation_path=? AND target_kind=? AND target_path=?)",
                params![relation.path, object_kind(&target.kind), target.path],
                |row| row.get(0),
            )?;
            if !linked {
                insert_ensure_link(tx, &relation, None, Some(target.clone()), now)?;
            }
        }
    }
    Ok(())
}

fn insert_ensure_link(
    tx: &Transaction<'_>,
    relation: &Relation,
    source: Option<ObjectRef>,
    target: Option<ObjectRef>,
    now: DateTime<Utc>,
) -> Result<(), StoreError> {
    let planned = PlannedLink {
        path: format!("{}/links/ensure-{}", relation.path, Uuid::new_v4()),
        source,
        relation_path: relation.path.clone(),
        target,
        spec: json!({ "state": kas_core::STATE_AVAILABLE }),
        status: json!({ "state": kas_core::STATE_PENDING }),
        metadata: json!({}),
    };
    insert_link(tx, &planned, false, now)?;
    let link = tx.query_row(LINK_SELECT_BY_ID, [&planned.path], link_from_row)?;
    append_lifecycle_event(
        tx,
        EventType::Created,
        ObjectKind::Link,
        &link.path,
        Some(link.revision),
        &link,
        now,
    )?;
    enqueue_link_if_drifted(tx, &link, "ensure_missing", now)
}

fn all_object_refs(connection: &Connection) -> Result<Vec<ObjectRef>, StoreError> {
    let mut statement = connection.prepare(
        "SELECT kind,id FROM (
           SELECT 'manifest' kind,id FROM manifests
           UNION ALL SELECT 'action',id FROM actions
           UNION ALL SELECT 'relation',id FROM relations
           UNION ALL SELECT 'resource',id FROM resources
           UNION ALL SELECT 'driver',id FROM drivers
           UNION ALL SELECT 'run',id FROM runs
           UNION ALL SELECT 'link',id FROM links
           UNION ALL SELECT 'user',id FROM users
           UNION ALL SELECT 'service_account',id FROM service_accounts
           UNION ALL SELECT 'role',id FROM roles
           UNION ALL SELECT 'role_binding',id FROM role_bindings
           UNION ALL SELECT 'credential',id FROM credentials
         ) ORDER BY kind,id",
    )?;
    let values = statement
        .query_map([], |row| {
            let kind: String = row.get(0)?;
            Ok(ObjectRef {
                kind: object_kind_from_str(&kind, 0)?,
                path: row.get(1)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(values)
}

fn validate_object_selector(selector: &ObjectSelector, depth: usize) -> Result<(), StoreError> {
    if depth > 16 {
        return Err(StoreError::Invalid(
            "Relation selector nesting exceeds 16 levels".into(),
        ));
    }
    if let Some(KindSelector::Many(kinds)) = &selector.kind {
        if kinds.is_empty() {
            return Err(StoreError::Invalid(
                "Relation kind selector cannot be an empty list".into(),
            ));
        }
    }
    if let Some(path) = &selector.path {
        kas_auth::validate_path_pattern(path).map_err(|error| {
            StoreError::Invalid(format!("Relation selector path {path} is invalid: {error}"))
        })?;
    }
    for link in &selector.links {
        validate_object_path("Relation selector Link path", &link.relation_path)?;
        if let Some(object) = &link.object {
            validate_object_selector(object, depth + 1)?;
        }
    }
    for alternative in &selector.any_of {
        validate_object_selector(alternative, depth + 1)?;
    }
    Ok(())
}

fn restart_policy(value: RestartPolicy) -> &'static str {
    match value {
        RestartPolicy::Never => "never",
        RestartPolicy::OnFailure => "on_failure",
        RestartPolicy::Always => "always",
    }
}

fn driver_runtime_from_str(value: &str, index: usize) -> rusqlite::Result<DriverRuntime> {
    match value {
        "process" => Ok(DriverRuntime::Process),
        other => Err(from_sql(index, format!("invalid Driver runtime {other}"))),
    }
}

fn restart_policy_from_str(value: &str, index: usize) -> rusqlite::Result<RestartPolicy> {
    match value {
        "never" => Ok(RestartPolicy::Never),
        "on_failure" => Ok(RestartPolicy::OnFailure),
        "always" => Ok(RestartPolicy::Always),
        other => Err(from_sql(index, format!("invalid restart policy {other}"))),
    }
}

fn validate_name(label: &str, value: &str) -> Result<(), StoreError> {
    if value.trim().is_empty() || value.chars().count() > 128 {
        return Err(StoreError::Invalid(format!("{label} is empty or too long")));
    }
    Ok(())
}

fn validate_permission_segment(label: &str, value: &str) -> Result<(), StoreError> {
    if value.contains('/') || value == "*" {
        return Err(StoreError::Invalid(format!(
            "{label} cannot contain / or equal *"
        )));
    }
    Ok(())
}

fn validate_object_path(label: &str, value: &str) -> Result<(), StoreError> {
    kas_auth::validate_path(value)
        .map_err(|error| StoreError::Invalid(format!("{label} is invalid: {error}")))
}

fn validate_rules(rules: &[Rule]) -> Result<(), StoreError> {
    for rule in rules {
        for path in &rule.paths {
            kas_auth::validate_path_pattern(path).map_err(|error| {
                StoreError::Invalid(format!("Rule path pattern {path} is invalid: {error}"))
            })?;
        }
    }
    Ok(())
}

fn stamp(value: DateTime<Utc>) -> String {
    value.to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
}

fn parse_stamp(value: &str) -> Result<DateTime<Utc>, StoreError> {
    DateTime::parse_from_rfc3339(value)
        .map(|value| value.with_timezone(&Utc))
        .map_err(|error| StoreError::Invalid(format!("invalid stored timestamp: {error}")))
}

fn uuid_from_row(row: &Row<'_>, index: usize) -> rusqlite::Result<Uuid> {
    parse_uuid(&row.get::<_, String>(index)?, index).map_err(|error| match error {
        StoreError::Database(error) => error,
        other => from_sql(index, other.to_string()),
    })
}

fn parse_uuid(value: &str, index: usize) -> Result<Uuid, StoreError> {
    Uuid::parse_str(value)
        .map_err(|error| StoreError::Database(from_sql(index, format!("invalid UUID: {error}"))))
}

fn json_from_row<T: serde::de::DeserializeOwned>(
    row: &Row<'_>,
    index: usize,
) -> rusqlite::Result<T> {
    serde_json::from_str(&row.get::<_, String>(index)?)
        .map_err(|error| from_sql(index, error.to_string()))
}

fn optional_json_from_row(row: &Row<'_>, index: usize) -> rusqlite::Result<Option<Value>> {
    row.get::<_, Option<String>>(index)?
        .map(|value| {
            serde_json::from_str(&value).map_err(|error| from_sql(index, error.to_string()))
        })
        .transpose()
}

fn time_from_row(row: &Row<'_>, index: usize) -> rusqlite::Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(&row.get::<_, String>(index)?)
        .map(|value| value.with_timezone(&Utc))
        .map_err(|error| from_sql(index, error.to_string()))
}

fn optional_time_from_row(row: &Row<'_>, index: usize) -> rusqlite::Result<Option<DateTime<Utc>>> {
    row.get::<_, Option<String>>(index)?
        .map(|value| {
            DateTime::parse_from_rfc3339(&value)
                .map(|value| value.with_timezone(&Utc))
                .map_err(|error| from_sql(index, error.to_string()))
        })
        .transpose()
}

fn from_sql(index: usize, message: String) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        index,
        rusqlite::types::Type::Text,
        Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            message,
        )),
    )
}

fn constraint(error: rusqlite::Error, message: &str) -> StoreError {
    match error {
        rusqlite::Error::SqliteFailure(details, _)
            if details.code == rusqlite::ErrorCode::ConstraintViolation =>
        {
            StoreError::Conflict(message.into())
        }
        other => StoreError::Database(other),
    }
}

fn configure(connection: &Connection) -> Result<(), StoreError> {
    connection.execute_batch("PRAGMA foreign_keys=ON;")?;
    Ok(())
}

fn schema_version(connection: &Connection) -> Result<u32, StoreError> {
    Ok(connection.query_row("PRAGMA user_version", [], |row| row.get(0))?)
}

fn require_current_schema(connection: &Connection) -> Result<(), StoreError> {
    let current = schema_version(connection)?;
    if current < LATEST_SCHEMA_VERSION {
        return Err(StoreError::MigrationRequired {
            current,
            latest: LATEST_SCHEMA_VERSION,
        });
    }
    if current > LATEST_SCHEMA_VERSION {
        return Err(StoreError::UnsupportedSchema {
            current,
            latest: LATEST_SCHEMA_VERSION,
        });
    }
    Ok(())
}

fn migrate_connection(connection: &mut Connection) -> Result<u32, StoreError> {
    let mut current = schema_version(connection)?;
    if current > LATEST_SCHEMA_VERSION {
        return Err(StoreError::UnsupportedSchema {
            current,
            latest: LATEST_SCHEMA_VERSION,
        });
    }
    for (version, sql) in MIGRATIONS {
        if *version <= current {
            continue;
        }
        if *version != current + 1 {
            return Err(StoreError::Invalid(format!(
                "missing migration from version {current} to {version}"
            )));
        }
        let tx = connection.transaction()?;
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", version)?;
        tx.commit()?;
        current = *version;
    }
    Ok(current)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn manifest(path: &str) -> CreateManifest {
        CreateManifest {
            path: path.into(),
            name: "note".into(),
            version: 1,
            description: "notes".into(),
            resource_schema: json!({"type": "object"}),
            states: vec![],
            default_state: kas_core::STATE_AVAILABLE.into(),
            initial_state: kas_core::STATE_AVAILABLE.into(),
            actions: Vec::new(),
            relations: Vec::new(),
            driver: None,
            rbac: ManifestRbac::default(),
            package_digest:
                "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
        }
    }

    #[test]
    fn migration_builds_path_schema() {
        let store = Store::memory().unwrap();
        assert_eq!(
            schema_version(&store.connection).unwrap(),
            LATEST_SCHEMA_VERSION
        );
        let columns: Vec<String> = {
            let mut statement = store
                .connection
                .prepare("PRAGMA table_info(events)")
                .unwrap();
            statement
                .query_map([], |row| row.get(1))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap()
        };
        assert!(columns.iter().any(|column| column == "object_path"));
        assert!(!columns.iter().any(|column| column == "object_id"));
        assert!(!columns.iter().any(|column| column == "manifest_path"));
        for table in [
            "packages",
            "actions",
            "relations",
            "resource_manifest_index",
            "run_relation_index",
        ] {
            let exists: bool = store
                .connection
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name=?)",
                    [table],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(exists, "missing table {table}");
        }
        let system_relations: u64 = store
            .connection
            .query_row(
                "SELECT COUNT(*) FROM relations WHERE protected=1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(system_relations, 8);
        let editor = store
            .get_role("/manifests/system/auth/roles/editor")
            .unwrap();
        assert!(editor.rules[0].verbs.iter().any(|verb| verb == "link"));
    }

    #[test]
    fn migration_is_data_free_and_store_installs_builtins() {
        let mut connection = Connection::open_in_memory().unwrap();
        configure(&connection).unwrap();
        migrate_connection(&mut connection).unwrap();
        let relation_count: u64 = connection
            .query_row("SELECT COUNT(*) FROM relations", [], |row| row.get(0))
            .unwrap();
        let role_count: u64 = connection
            .query_row("SELECT COUNT(*) FROM roles", [], |row| row.get(0))
            .unwrap();
        assert_eq!((relation_count, role_count), (0, 0));

        let store = Store::memory().unwrap();
        assert!(relation_path_by_role(&store.connection, RelationRole::ResourceManifest).is_ok());
        let admin_path: String = store
            .connection
            .query_row(
                "SELECT id FROM roles WHERE system_role='admin'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(admin_path, "/manifests/system/auth/roles/admin");
    }

    #[test]
    fn bootstrap_admin_uses_builtin_role_relationships() {
        let mut store = Store::memory().unwrap();
        let credential = store.bootstrap_admin("root").unwrap();
        let authenticated = store.authenticate(&credential.token).unwrap();
        assert!(authenticated
            .rules
            .iter()
            .any(|rule| rule.resources == ["*"] && rule.verbs == ["*"]));
        let role_path: String = store
            .connection
            .query_row(
                "SELECT role_path FROM role_binding_role_index
                 WHERE role_binding_path='/role-bindings/system/bootstrap-admin'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(role_path, "/manifests/system/auth/roles/admin");
    }

    #[test]
    fn manifest_rbac_is_installed_with_protected_relationships() {
        let mut store = Store::memory().unwrap();
        let manifest_path = "/manifests/worker/v1";
        let service_account_path = format!("{manifest_path}/service-accounts/runtime");
        let role_path = format!("{manifest_path}/roles/runtime");
        let binding_path = format!("{manifest_path}/role-bindings/runtime");
        let mut input = manifest(manifest_path);
        input.name = "worker".into();
        input.rbac.service_accounts.push(ServiceAccountDefinition {
            path: service_account_path.clone(),
            name: "worker-runtime".into(),
        });
        input.rbac.roles.push(RoleDefinition {
            path: role_path.clone(),
            name: "worker-runtime".into(),
            description: String::new(),
            rules: vec![RbacRuleDefinition {
                resources: vec!["resources/*".into()],
                verbs: vec!["create".into()],
                paths: Vec::new(),
            }],
            system_role: None,
        });
        input.rbac.role_bindings.push(RoleBindingDefinition {
            path: binding_path.clone(),
            name: "worker-runtime".into(),
            role_path: role_path.clone(),
            subjects: vec![RbacSubjectDefinition {
                kind: RbacSubjectKind::ServiceAccount,
                path: service_account_path.clone(),
            }],
        });

        let installed = store.install_manifest(input, 123).unwrap();
        assert_eq!(installed.rbac.service_accounts.len(), 1);
        assert_eq!(installed.rbac.roles.len(), 1);
        assert_eq!(installed.rbac.role_bindings.len(), 1);
        let managed_by = format!("package:{manifest_path}");
        assert_eq!(store.get_role(&role_path).unwrap().managed_by, managed_by);
        let protected_links: u64 = store
            .connection
            .query_row(
                "SELECT COUNT(*) FROM links
                 WHERE protected=1 AND (
                    source_path=? OR target_path=? OR source_path=? OR target_path=?
                 )",
                params![
                    binding_path,
                    binding_path,
                    service_account_path,
                    service_account_path
                ],
                |row| row.get(0),
            )
            .unwrap();
        assert!(protected_links >= 4);
    }

    #[test]
    fn objects_are_addressed_by_path_and_emit_path_events() {
        let mut store = Store::memory().unwrap();
        let created_manifest = store
            .install_manifest(manifest("/manifests/note/v1"), 123)
            .unwrap();
        assert_eq!(created_manifest.path, "/manifests/note/v1");

        let resource = store
            .create_resource(CreateResource {
                path: "/notes/team-a/first".into(),
                manifest: created_manifest.path,
                name: "first".into(),
                spec: json!({"body": "hello"}),
                links: Vec::new(),
            })
            .unwrap();
        assert_eq!(store.get_resource("/notes/team-a/first").unwrap(), resource);
        let events = store.list_events(Some(0), 10).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].object_path, resource.path);
    }

    #[test]
    fn invalid_object_paths_are_rejected() {
        let mut store = Store::memory().unwrap();
        let error = store
            .install_manifest(manifest("/manifests//note"), 123)
            .unwrap_err();
        assert!(matches!(error, StoreError::Invalid(_)));
    }

    #[test]
    fn manifest_install_is_idempotent_only_for_the_same_package() {
        let mut store = Store::memory().unwrap();
        let input = manifest("/manifests/note/v1");
        let first = store.install_manifest(input.clone(), 123).unwrap();
        let second = store.install_manifest(input, 123).unwrap();
        assert_eq!(first, second);

        let mut changed = manifest("/manifests/note/v1");
        changed.package_digest =
            "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into();
        assert!(matches!(
            store.install_manifest(changed, 123),
            Err(StoreError::Conflict(_))
        ));
    }

    #[test]
    fn driver_identity_and_credentials_use_paths() {
        let mut store = Store::memory().unwrap();
        let mut input = manifest("/manifests/note/v1");
        let service_account = "/manifests/note/v1/service-accounts/driver";
        input.rbac.service_accounts.push(ServiceAccountDefinition {
            path: service_account.into(),
            name: "note-driver".into(),
        });
        input.driver = Some(DriverDefinition {
            path: "/manifests/note/v1/driver".into(),
            runtime: DriverRuntime::Process,
            entrypoint: "./driver/bin/note".into(),
            service_account: service_account.into(),
            args: Vec::new(),
            restart: RestartPolicy::OnFailure,
        });
        store.install_manifest(input, 123).unwrap();

        let driver = store
            .driver_for_manifest("/manifests/note/v1")
            .unwrap()
            .unwrap();
        assert_eq!(driver.path, "/manifests/note/v1/driver");
        let driver = store.start_driver(&driver.path).unwrap();
        let credential = store.issue_driver_credential(&driver.path).unwrap();
        assert!(credential
            .path
            .starts_with("/manifests/note/v1/service-accounts/driver/credentials/"));

        let authenticated = store.authenticate(&credential.token).unwrap();
        assert_eq!(
            authenticated.subject.path,
            "/manifests/note/v1/service-accounts/driver"
        );
        assert_eq!(
            authenticated.driver_path.as_deref(),
            Some("/manifests/note/v1/driver")
        );
    }

    #[test]
    fn role_path_scopes_round_trip_and_are_validated() {
        let mut store = Store::memory().unwrap();
        let role = store
            .create_role(CreateRole {
                path: "/roles/team-a/note-reader".into(),
                name: "note-reader".into(),
                description: "read team notes".into(),
                rules: vec![Rule {
                    resources: vec!["resources/note".into()],
                    verbs: vec!["get".into(), "patch".into()],
                    paths: vec!["/notes/team-a/**".into()],
                }],
            })
            .unwrap();
        assert_eq!(
            store
                .list_roles()
                .unwrap()
                .into_iter()
                .find(|candidate| candidate.path == role.path)
                .unwrap()
                .rules[0]
                .paths,
            vec!["/notes/team-a/**"]
        );

        let error = store
            .create_role(CreateRole {
                path: "/roles/team-a/invalid".into(),
                name: "invalid".into(),
                description: String::new(),
                rules: vec![Rule {
                    resources: vec!["resources/note".into()],
                    verbs: vec!["get".into()],
                    paths: vec!["notes/**".into()],
                }],
            })
            .unwrap_err();
        assert!(matches!(error, StoreError::Invalid(_)));
    }

    #[test]
    fn links_accept_identity_and_rbac_objects_as_endpoints() {
        let mut store = Store::memory().unwrap();
        let mut input = manifest("/manifests/security/v1");
        input.name = "security".into();
        input.relations.push(Relation {
            path: "/manifests/security/v1/relations/related-to".into(),
            name: "related_to".into(),
            role: None,
            inverse_name: None,
            sources: vec![ObjectSelector::default()],
            targets: vec![ObjectSelector::default()],
            relation_type: RelationType::ManyToMany,
            ensure: false,
            on_source_delete: OnSourceDelete::Unlink,
            metadata_schema: json!({"type":"object"}),
        });
        store.install_manifest(input, 123).unwrap();
        let user = store
            .create_user(CreateUser {
                path: "/users/alice".into(),
                name: "alice".into(),
            })
            .unwrap();
        let service_account = store
            .create_service_account(CreateServiceAccount {
                path: "/service-accounts/automation".into(),
                name: "automation".into(),
            })
            .unwrap();
        let role = store
            .create_role(CreateRole {
                path: "/roles/automation".into(),
                name: "automation".into(),
                description: "automation role".into(),
                rules: Vec::new(),
            })
            .unwrap();
        let role_binding = store
            .create_role_binding(CreateRoleBinding {
                path: "/role-bindings/automation".into(),
                name: "automation".into(),
                role_path: role.path.clone(),
                subjects: vec![Subject {
                    kind: SubjectKind::ServiceAccount,
                    path: service_account.path.clone(),
                }],
            })
            .unwrap();
        let credential = store
            .issue_service_account_credential(&service_account.path)
            .unwrap();

        let endpoints = [
            (ObjectKind::User, user.path),
            (ObjectKind::ServiceAccount, service_account.path),
            (ObjectKind::Role, role.path.clone()),
            (ObjectKind::RoleBinding, role_binding.path),
            (ObjectKind::Credential, credential.path),
        ];
        for (index, (kind, path)) in endpoints.into_iter().enumerate() {
            let link = store
                .create_link(CreateLink {
                    path: format!("/links/security-object-{index}"),
                    source: Some(ObjectRef { kind, path }),
                    relation_path: "/manifests/security/v1/relations/related-to".into(),
                    target: Some(ObjectRef {
                        kind: ObjectKind::Role,
                        path: role.path.clone(),
                    }),
                    spec: json!({"state":"available"}),
                    status: json!({"state":"available"}),
                    metadata: json!({}),
                })
                .unwrap();
            assert_eq!(store.get_link(&link.path).unwrap(), link);
        }
    }

    #[test]
    fn ensured_link_reconciles_and_deleted_resource_is_hard_removed() {
        let mut store = Store::memory().unwrap();
        let mut input = manifest("/manifests/agent/v1");
        input.name = "agent".into();
        input.initial_state = kas_core::STATE_PENDING.into();
        input.driver = Some(DriverDefinition {
            path: "/manifests/agent/v1/driver".into(),
            runtime: DriverRuntime::Process,
            entrypoint: "./driver".into(),
            service_account: "/manifests/agent/v1/service-accounts/driver".into(),
            args: vec![],
            restart: RestartPolicy::Never,
        });
        input.rbac.service_accounts.push(ServiceAccountDefinition {
            path: "/manifests/agent/v1/service-accounts/driver".into(),
            name: "agent-driver".into(),
        });
        input.rbac.roles.push(RoleDefinition {
            path: "/manifests/agent/v1/roles/runtime".into(),
            name: "agent-runtime".into(),
            description: String::new(),
            rules: Vec::new(),
            system_role: None,
        });
        input.relations.push(Relation {
            path: "/manifests/agent/v1/relations/account".into(),
            name: "account".into(),
            role: None,
            inverse_name: Some("agent".into()),
            sources: vec![ObjectSelector {
                kind: Some(KindSelector::One(ObjectKind::Resource)),
                path: Some("/resources/agent-a".into()),
                ..ObjectSelector::default()
            }],
            targets: vec![ObjectSelector {
                kind: Some(KindSelector::One(ObjectKind::ServiceAccount)),
                path: Some("/service-accounts/agent-a".into()),
                ..ObjectSelector::default()
            }],
            relation_type: RelationType::OneToOne,
            ensure: true,
            on_source_delete: OnSourceDelete::Cascade,
            metadata_schema: json!({}),
        });
        store.install_manifest(input, 123).unwrap();
        let driver = store.start_driver("/manifests/agent/v1/driver").unwrap();
        store
            .mark_driver_ready(
                &driver.path,
                DriverReady {
                    generation: driver.generation,
                    process_id: 42,
                    metadata: json!({}),
                },
            )
            .unwrap();
        store
            .create_resource(CreateResource {
                path: "/resources/agent-a".into(),
                manifest: "/manifests/agent/v1".into(),
                name: "agent-a".into(),
                spec: json!({}),
                links: vec![],
            })
            .unwrap();

        let pending = store
            .list_links(LinkFilter {
                relation_path: Some("/manifests/agent/v1/relations/account".into()),
                ..LinkFilter::default()
            })
            .unwrap();
        assert_eq!(pending.len(), 1);
        assert!(pending[0].source.is_some());
        assert!(pending[0].target.is_none());
        assert_eq!(pending[0].status, json!({"state":"pending"}));

        for _ in 0..2 {
            let delivery = store
                .claim_driver_delivery(&driver.path, driver.generation)
                .unwrap()
                .unwrap();
            match &delivery.work {
                DriverWork::Reconcile {
                    object: ReconcileObject::Resource(resource),
                } => {
                    store
                        .finish_reconciliation_with_mutations(
                            delivery.id,
                            &driver.path,
                            driver.generation,
                            vec![Mutation::UpdateResourceStatus {
                                resource_path: resource.path.clone(),
                                expected_revision: resource.revision,
                                status: resource.spec.clone(),
                            }],
                        )
                        .unwrap();
                }
                DriverWork::Reconcile {
                    object: ReconcileObject::Link(link),
                } => {
                    store
                        .finish_reconciliation_with_mutations(
                            delivery.id,
                            &driver.path,
                            driver.generation,
                            vec![
                                Mutation::CreateServiceAccount {
                                    path: "/service-accounts/agent-a".into(),
                                    name: "agent-a".into(),
                                },
                                Mutation::CreateRoleBinding {
                                    path: "/role-bindings/agent-a".into(),
                                    name: "agent-a".into(),
                                    role_path: "/manifests/agent/v1/roles/runtime".into(),
                                    subjects: vec![RbacSubjectDefinition {
                                        kind: RbacSubjectKind::ServiceAccount,
                                        path: "/service-accounts/agent-a".into(),
                                    }],
                                },
                                Mutation::UpdateLink {
                                    link_path: link.path.clone(),
                                    expected_revision: link.revision,
                                    source: link.source.clone(),
                                    target: Some(ObjectRef {
                                        kind: ObjectKind::ServiceAccount,
                                        path: "/service-accounts/agent-a".into(),
                                    }),
                                    status: link.spec.clone(),
                                },
                            ],
                        )
                        .unwrap();
                }
                DriverWork::Run { .. } => panic!("unexpected Run delivery"),
            }
        }

        let resource = store.get_resource("/resources/agent-a").unwrap();
        assert_eq!(resource.spec, resource.status);
        let link = store
            .list_links(LinkFilter {
                relation_path: Some("/manifests/agent/v1/relations/account".into()),
                ..LinkFilter::default()
            })
            .unwrap()
            .pop()
            .unwrap();
        assert_eq!(link.spec, link.status);
        assert!(link.target.is_some());
        assert_eq!(
            store
                .list_role_bindings()
                .unwrap()
                .into_iter()
                .filter(|binding| binding.path == "/role-bindings/agent-a")
                .count(),
            1
        );

        let deleting = store
            .delete_resource("/resources/agent-a", resource.revision)
            .unwrap();
        let deleted_status = deleting.spec.clone();
        store
            .update_resource_status(
                "/resources/agent-a",
                UpdateResourceStatus {
                    driver_path: driver.path,
                    driver_generation: driver.generation,
                    expected_revision: deleting.revision,
                    status: deleted_status,
                },
            )
            .unwrap();
        assert!(matches!(
            store.get_resource("/resources/agent-a"),
            Err(StoreError::NotFound(_))
        ));
        assert!(matches!(
            store.get_service_account("/service-accounts/agent-a"),
            Err(StoreError::NotFound(_))
        ));
        assert!(!store
            .list_role_bindings()
            .unwrap()
            .into_iter()
            .any(|binding| binding.path == "/role-bindings/agent-a"));
        assert!(store
            .list_links(LinkFilter {
                relation_path: Some("/manifests/agent/v1/relations/account".into()),
                ..LinkFilter::default()
            })
            .unwrap()
            .is_empty());
    }

    #[test]
    fn run_relationships_are_projected_and_action_input_is_validated() {
        let mut store = Store::memory().unwrap();
        let manifest_path = "/manifests/note/v1";
        let driver_path = format!("{manifest_path}/driver");
        let action_path = format!("{manifest_path}/actions/render");
        let mut input = manifest(manifest_path);
        input.actions.push(Action {
            path: action_path.clone(),
            name: "render".into(),
            description: String::new(),
            input_schema: json!({
                "type": "object",
                "required": ["body"],
                "properties": {"body": {"type": "string"}}
            }),
            output_schema: json!({"type":"object"}),
        });
        input.driver = Some(DriverDefinition {
            path: driver_path.clone(),
            runtime: DriverRuntime::Process,
            entrypoint: "./driver/bin/note".into(),
            service_account: format!("{manifest_path}/service-accounts/driver"),
            args: Vec::new(),
            restart: RestartPolicy::OnFailure,
        });
        input.rbac.service_accounts.push(ServiceAccountDefinition {
            path: format!("{manifest_path}/service-accounts/driver"),
            name: "note-driver".into(),
        });
        store.install_manifest(input, 123).unwrap();

        let resource_path = "/notes/first";
        store
            .create_resource(CreateResource {
                path: resource_path.into(),
                manifest: manifest_path.into(),
                name: "first".into(),
                spec: json!({}),
                links: Vec::new(),
            })
            .unwrap();
        let request_id = Uuid::new_v4();
        let run_path = format!("{resource_path}/runs/{request_id}");
        let run = store
            .enqueue_run(CreateRun {
                path: run_path.clone(),
                request_id,
                resource: resource_path.into(),
                action: action_path.clone(),
                input: json!({"body":"hello"}),
                links: Vec::new(),
            })
            .unwrap();
        assert_eq!(run.path, run_path);
        let projection: (String, String, String) = store
            .connection
            .query_row(
                "SELECT resource_path,action_path,driver_path FROM run_relation_index WHERE run_path=?",
                [&run.path],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            projection,
            (resource_path.into(), action_path.clone(), driver_path)
        );

        let invalid_request_id = Uuid::new_v4();
        let invalid_run_path = format!("{resource_path}/runs/{invalid_request_id}");
        assert!(matches!(
            store.enqueue_run(CreateRun {
                path: invalid_run_path,
                request_id: invalid_request_id,
                resource: resource_path.into(),
                action: action_path,
                input: json!({"body": 42}),
                links: Vec::new(),
            }),
            Err(StoreError::Invalid(_))
        ));
    }

    #[test]
    fn generic_object_listing_and_lookup_cover_manifest_objects() {
        let mut store = Store::memory().unwrap();
        store
            .install_manifest(manifest("/manifests/object-test/v1"), 123)
            .unwrap();

        let manifests = store.list_objects(Some(ObjectKind::Manifest)).unwrap();
        assert!(manifests.iter().any(|object| {
            object.kind == ObjectKind::Manifest && object.path == "/manifests/object-test/v1"
        }));

        let value = store
            .object_value(&ObjectRef {
                kind: ObjectKind::Manifest,
                path: "/manifests/object-test/v1".into(),
            })
            .unwrap();
        assert_eq!(value["path"], "/manifests/object-test/v1");
    }
}
