use std::{
    collections::HashSet,
    sync::{Arc, Mutex},
    time::Duration,
};

use kas_api::app;
use kas_auth::{
    CreateRole, CreateRoleBinding, CreateServiceAccount, CreateUser, IssuedCredential, Role, Rule,
    ServiceAccount, Subject, SubjectKind, User,
};
use kas_core::{
    Action, CreateLink, CreateManifest, CreateResource, CreateRun, Driver as DriverRecord,
    DriverState, Link, Manifest, ObjectKind, ObjectRef, Resource, Run, RunStatus,
};
use kas_driver::{
    Driver as DriverImplementation, DriverError, DriverRuntime, WatchEvent, WatchSelector,
};
use kas_store::Store;
use kas_test_driver::TestDriver;
use reqwest::{header, Client};
use serde_json::json;
use tokio::sync::Notify;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use uuid::Uuid;

#[derive(Default)]
struct ObservedWatch {
    events: Mutex<Vec<WatchEvent>>,
    changed: Notify,
}

struct RecordingTestDriver {
    selectors: Vec<WatchSelector>,
    observed: Arc<ObservedWatch>,
}

impl DriverImplementation for RecordingTestDriver {
    fn name(&self) -> &str {
        DriverImplementation::name(&TestDriver)
    }

    fn reconcile(&self, resource: &Resource) -> Result<serde_json::Value, DriverError> {
        DriverImplementation::reconcile(&TestDriver, resource)
    }

    fn execute(
        &self,
        resource: &Resource,
        run: &Run,
    ) -> Result<kas_core::DriverExecution, DriverError> {
        DriverImplementation::execute(&TestDriver, resource, run)
    }

    fn watch_selectors(&self) -> Vec<WatchSelector> {
        self.selectors.clone()
    }

    fn on_watch_event(&self, event: &WatchEvent) -> Result<(), DriverError> {
        self.observed.events.lock().unwrap().push(event.clone());
        self.observed.changed.notify_waiters();
        Ok(())
    }
}

async fn wait_for_watch_event(
    observed: &ObservedWatch,
    matches: impl Fn(&WatchEvent) -> bool,
) -> anyhow::Result<WatchEvent> {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let changed = observed.changed.notified();
            if let Some(event) = observed
                .events
                .lock()
                .unwrap()
                .iter()
                .find(|event| matches(event))
                .cloned()
            {
                return event;
            }
            changed.await;
        }
    })
    .await
    .map_err(Into::into)
}

fn watch_event_parts(event: &WatchEvent) -> (&'static str, u64, &kas_driver::WatchObject) {
    match event {
        WatchEvent::Created { cursor, object, .. } => ("created", *cursor, object),
        WatchEvent::Updated { cursor, object, .. } => ("updated", *cursor, object),
        WatchEvent::Deleted { cursor, object, .. } => ("deleted", *cursor, object),
    }
}

fn with_path(url: impl AsRef<str>, path: &str) -> String {
    format!("{}?path={path}", url.as_ref())
}

fn authenticated_client(token: &str) -> anyhow::Result<Client> {
    let mut headers = header::HeaderMap::new();
    headers.insert(header::AUTHORIZATION, format!("Bearer {token}").parse()?);
    Ok(Client::builder().default_headers(headers).build()?)
}

async fn replace_driver_connection(
    address: std::net::SocketAddr,
    driver_path: &str,
    generation: u64,
    token: &str,
) -> anyhow::Result<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
> {
    let mut request =
        format!("ws://{address}/drivers/connect?path={driver_path}&generation={generation}")
            .into_client_request()?;
    request
        .headers_mut()
        .insert(header::AUTHORIZATION, format!("Bearer {token}").parse()?);
    Ok(tokio_tungstenite::connect_async(request).await?.0)
}

async fn wait_for_finished_run(client: &Client, api: &str, path: &str) -> anyhow::Result<Run> {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let run: Run = client
                .get(with_path(format!("{api}/runs/by-path"), path))
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            if !matches!(run.status, RunStatus::Queued | RunStatus::Running) {
                return Ok::<Run, anyhow::Error>(run);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?
}

#[tokio::test]
async fn driver_path_permissions_and_websocket_work_end_to_end() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let database = directory.path().join("kas.db");
    kas_store::migrate(&database)?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let mut store = Store::open(&database)?;
    let admin = store.bootstrap_admin("admin")?;
    let application = app(store);
    let server = tokio::spawn(async move { axum::serve(listener, application).await });
    let api = format!("http://{address}");

    assert_eq!(
        Client::new()
            .get(format!("{api}/resources"))
            .send()
            .await?
            .status(),
        reqwest::StatusCode::UNAUTHORIZED
    );
    let client = authenticated_client(&admin.token)?;

    let note_manifest: Manifest = client
        .post(format!("{api}/manifests"))
        .json(&CreateManifest {
            path: "/manifests/note".into(),
            name: "note".into(),
            version: 1,
            description: "Passive output".into(),
            resource_schema: json!({
                "type": "object",
                "properties": { "archived": { "type": "boolean" } },
                "required": ["archived"],
                "additionalProperties": false
            }),
            actions: vec![],
            driver: None,
        })
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert!(client
        .get(with_path(
            format!("{api}/manifests/driver"),
            &note_manifest.path
        ))
        .send()
        .await?
        .error_for_status()?
        .json::<Option<DriverRecord>>()
        .await?
        .is_none());

    // One scoped reader demonstrates exact, one-segment `*`, and descendant
    // `**` matching through the real REST list filter.
    let fixtures = [
        "/scope/exact",
        "/scope/star/one",
        "/scope/star/one/deep",
        "/scope/deep",
        "/scope/deep/a/b",
        "/scope/other",
    ];
    for path in fixtures {
        client
            .post(format!("{api}/resources"))
            .json(&CreateResource {
                path: path.into(),
                manifest_path: note_manifest.path.clone(),
                name: path.rsplit('/').next().unwrap().into(),
                spec: json!({ "archived": false }),
            })
            .send()
            .await?
            .error_for_status()?;
    }
    let reader: User = client
        .post(format!("{api}/users"))
        .json(&CreateUser {
            path: "/users/scoped-reader".into(),
            name: "scoped-reader".into(),
        })
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let reader_role: Role = client
        .post(format!("{api}/roles"))
        .json(&CreateRole {
            path: "/roles/readers/scoped".into(),
            name: "scoped-reader".into(),
            description: "Exercise all supported path patterns".into(),
            rules: vec![Rule {
                resources: vec!["resources/note".into()],
                verbs: vec!["list".into()],
                paths: vec![
                    "/scope/exact".into(),
                    "/scope/star/*".into(),
                    "/scope/deep/**".into(),
                ],
            }],
        })
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    client
        .post(format!("{api}/role-bindings"))
        .json(&CreateRoleBinding {
            path: "/role-bindings/readers/scoped".into(),
            name: "scoped-reader".into(),
            role_path: reader_role.path,
            subjects: vec![Subject {
                kind: SubjectKind::User,
                path: reader.path.clone(),
            }],
        })
        .send()
        .await?
        .error_for_status()?;
    let reader_credential: IssuedCredential = client
        .post(with_path(format!("{api}/users/credentials"), &reader.path))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let visible: HashSet<String> = authenticated_client(&reader_credential.token)?
        .get(format!("{api}/resources"))
        .send()
        .await?
        .error_for_status()?
        .json::<Vec<Resource>>()
        .await?
        .into_iter()
        .map(|resource| resource.path)
        .collect();
    assert_eq!(
        visible,
        [
            "/scope/exact",
            "/scope/star/one",
            "/scope/deep",
            "/scope/deep/a/b"
        ]
        .into_iter()
        .map(str::to_owned)
        .collect()
    );

    let manifest: Manifest = client
        .post(format!("{api}/manifests"))
        .json(&CreateManifest {
            path: "/manifests/test".into(),
            name: "test".into(),
            version: 1,
            description: "End-to-end test Manifest".into(),
            resource_schema: json!({ "type": "object" }),
            actions: vec![Action {
                name: "echo".into(),
                description: "Echo the input".into(),
                input_schema: json!({
                    "type": "object",
                    "properties": { "message": { "type": "string" } },
                    "required": ["message"],
                    "additionalProperties": false
                }),
                output_schema: json!({ "type": "object" }),
            }],
            driver: Some("test-driver".into()),
        })
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let driver: DriverRecord = client
        .get(with_path(format!("{api}/manifests/driver"), &manifest.path))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let driver_service_account = client
        .get(format!("{api}/service-accounts"))
        .send()
        .await?
        .error_for_status()?
        .json::<Vec<ServiceAccount>>()
        .await?
        .into_iter()
        .find(|account| account.driver_path.as_deref() == Some(&driver.path))
        .expect("Driver ServiceAccount");

    // This binding lets the Driver fan out transactionally, watch only its
    // subtree, and manage ordinary identities/RBAC inside its delegated paths.
    let delegated_role: Role = client
        .post(format!("{api}/roles"))
        .json(&CreateRole {
            path: "/roles/system/test-driver-delegated".into(),
            name: "test-driver-delegated".into(),
            description: "Driver data and delegated control-plane scope".into(),
            rules: vec![
                Rule {
                    resources: vec!["resources/note".into()],
                    verbs: vec![
                        "create".into(),
                        "get".into(),
                        "link".into(),
                        "list".into(),
                        "watch".into(),
                    ],
                    paths: vec![
                        "/executions/team-a/**".into(),
                        "/watch/exact".into(),
                        "/watch/star/*".into(),
                        "/watch/deep/**".into(),
                    ],
                },
                Rule {
                    resources: vec!["resources/test".into()],
                    verbs: vec!["link".into(), "watch".into()],
                    paths: vec!["/executions/team-a/**".into()],
                },
                Rule {
                    resources: vec!["links".into()],
                    verbs: vec!["create".into(), "watch".into()],
                    paths: vec!["/executions/team-a/**".into()],
                },
                Rule {
                    resources: vec!["runs".into()],
                    verbs: vec!["link".into(), "watch".into()],
                    paths: vec!["/executions/team-a/**".into()],
                },
                Rule {
                    resources: vec!["serviceaccounts".into()],
                    verbs: vec!["create".into(), "link".into()],
                    paths: vec!["/service-accounts/team-a/**".into()],
                },
                Rule {
                    resources: vec!["credentials".into()],
                    verbs: vec!["create".into()],
                    paths: vec!["/service-accounts/team-a/**".into()],
                },
                Rule {
                    resources: vec!["roles".into()],
                    verbs: vec!["create".into()],
                    paths: vec!["/roles/team-a/**".into()],
                },
                Rule {
                    resources: vec!["rolebindings".into()],
                    verbs: vec!["create".into()],
                    paths: vec!["/role-bindings/team-a/**".into()],
                },
            ],
        })
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    client
        .post(format!("{api}/role-bindings"))
        .json(&CreateRoleBinding {
            path: "/role-bindings/system/test-driver-delegated".into(),
            name: "test-driver-delegated".into(),
            role_path: delegated_role.path,
            subjects: vec![Subject {
                kind: SubjectKind::ServiceAccount,
                path: driver_service_account.path.clone(),
            }],
        })
        .send()
        .await?
        .error_for_status()?;

    let starting: DriverRecord = client
        .patch(with_path(format!("{api}/drivers/by-path"), &driver.path))
        .json(&json!({ "state": "starting" }))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(starting.state, DriverState::Starting);
    let credential: IssuedCredential = client
        .post(with_path(
            format!("{api}/drivers/credentials"),
            &driver.path,
        ))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let driver_client = authenticated_client(&credential.token)?;

    // A Driver uses the same REST and RBAC model as any other ServiceAccount.
    let child_account: ServiceAccount = driver_client
        .post(format!("{api}/service-accounts"))
        .json(&CreateServiceAccount {
            path: "/service-accounts/team-a/worker".into(),
            name: "worker".into(),
        })
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let overreach = driver_client
        .post(format!("{api}/roles"))
        .json(&CreateRole {
            path: "/roles/team-a/overreach".into(),
            name: "overreach".into(),
            description: "Must be rejected".into(),
            rules: vec![Rule {
                resources: vec!["resources/note".into()],
                verbs: vec!["get".into()],
                paths: vec!["/executions/**".into()],
            }],
        })
        .send()
        .await?;
    assert_eq!(overreach.status(), reqwest::StatusCode::FORBIDDEN);
    let child_role: Role = driver_client
        .post(format!("{api}/roles"))
        .json(&CreateRole {
            path: "/roles/team-a/note-reader".into(),
            name: "note-reader".into(),
            description: "A strict subset of the Driver's own rights".into(),
            rules: vec![Rule {
                resources: vec!["resources/note".into()],
                verbs: vec!["get".into(), "watch".into()],
                paths: vec!["/executions/team-a/readonly/**".into()],
            }],
        })
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    driver_client
        .post(format!("{api}/role-bindings"))
        .json(&CreateRoleBinding {
            path: "/role-bindings/team-a/note-reader".into(),
            name: "note-reader".into(),
            role_path: child_role.path,
            subjects: vec![Subject {
                kind: SubjectKind::ServiceAccount,
                path: child_account.path.clone(),
            }],
        })
        .send()
        .await?
        .error_for_status()?;
    let child_credential: IssuedCredential = driver_client
        .post(with_path(
            format!("{api}/service-accounts/credentials"),
            &child_account.path,
        ))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert!(child_credential.token.starts_with("kas_"));

    let source_path = "/executions/team-a/test-1";
    let resource: Resource = client
        .post(format!("{api}/resources"))
        .json(&CreateResource {
            path: source_path.into(),
            manifest_path: manifest.path.clone(),
            name: "fixture".into(),
            spec: json!({
                "label": "fixture",
                "fanout_manifest_path": note_manifest.path
            }),
        })
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let initial_note: Resource = client
        .post(format!("{api}/resources"))
        .json(&CreateResource {
            path: "/executions/team-a/note-1".into(),
            manifest_path: note_manifest.path.clone(),
            name: "note-1".into(),
            spec: json!({ "archived": false }),
        })
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let forbidden_endpoint_link = driver_client
        .post(format!("{api}/links"))
        .json(&CreateLink {
            path: "/executions/team-a/links/forbidden-endpoint".into(),
            source: ObjectRef {
                kind: ObjectKind::ServiceAccount,
                path: child_account.path.clone(),
            },
            relation: "manages".into(),
            target: ObjectRef {
                kind: ObjectKind::Resource,
                path: "/scope/exact".into(),
            },
            metadata: json!({}),
        })
        .send()
        .await?;
    assert_eq!(
        forbidden_endpoint_link.status(),
        reqwest::StatusCode::FORBIDDEN
    );
    let managed_link: Link = driver_client
        .post(format!("{api}/links"))
        .json(&CreateLink {
            path: "/executions/team-a/links/managed-by-worker".into(),
            source: ObjectRef {
                kind: ObjectKind::ServiceAccount,
                path: child_account.path.clone(),
            },
            relation: "manages".into(),
            target: ObjectRef {
                kind: ObjectKind::Resource,
                path: resource.path.clone(),
            },
            metadata: json!({}),
        })
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(managed_link.source.kind, ObjectKind::ServiceAccount);
    let link: Link = client
        .post(format!("{api}/links"))
        .json(&CreateLink {
            path: "/executions/team-a/links/related".into(),
            source: ObjectRef {
                kind: ObjectKind::Resource,
                path: initial_note.path.clone(),
            },
            relation: "related_to".into(),
            target: ObjectRef {
                kind: ObjectKind::Resource,
                path: resource.path.clone(),
            },
            metadata: json!({ "reason": "e2e" }),
        })
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    let observed = Arc::new(ObservedWatch::default());
    let runtime = DriverRuntime::new(
        &api,
        driver.path.clone(),
        starting.generation,
        credential.token.clone(),
        RecordingTestDriver {
            selectors: vec![
                WatchSelector::Resource {
                    manifest_path: Some(manifest.path.clone()),
                    path: Some("/executions/team-a/**".into()),
                },
                WatchSelector::Resource {
                    manifest_path: Some(note_manifest.path.clone()),
                    path: Some("/executions/team-a/**".into()),
                },
                WatchSelector::Resource {
                    manifest_path: Some(note_manifest.path.clone()),
                    path: Some("/watch/exact".into()),
                },
                WatchSelector::Resource {
                    manifest_path: Some(note_manifest.path.clone()),
                    path: Some("/watch/star/*".into()),
                },
                WatchSelector::Resource {
                    manifest_path: Some(note_manifest.path.clone()),
                    path: Some("/watch/deep/**".into()),
                },
                WatchSelector::Link {
                    path: Some("/executions/team-a/**".into()),
                    relation: None,
                    source: None,
                    target: None,
                },
                WatchSelector::Run {
                    resource_path: Some(resource.path.clone()),
                    path: Some("/executions/team-a/**".into()),
                },
            ],
            observed: observed.clone(),
        },
    )
    .with_reconnect_interval(Duration::from_millis(200));
    let driver_process = tokio::spawn(async move { runtime.run().await });

    let request_id = Uuid::new_v4();
    let run_path = format!("{source_path}/runs/{request_id}");
    let first: Run = client
        .post(format!("{api}/runs"))
        .json(&CreateRun {
            path: run_path.clone(),
            request_id,
            resource_path: resource.path.clone(),
            action: "echo".into(),
            input: json!({ "message": "hello" }),
        })
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(first.path, run_path);
    let first = wait_for_finished_run(&client, &api, &first.path).await?;
    assert_eq!(first.status, RunStatus::Succeeded);
    assert_eq!(
        first.output.as_ref().unwrap()["echo"],
        json!({ "message": "hello" })
    );
    let fanout_path = first.output.as_ref().unwrap()["fanout_resource_path"]
        .as_str()
        .unwrap()
        .to_owned();
    let fanout: Resource = client
        .get(with_path(format!("{api}/resources/by-path"), &fanout_path))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(fanout.manifest_path, note_manifest.path);
    let produced: Vec<Link> = client
        .get(format!(
            "{api}/links?source_kind=run&source_path={}",
            first.path
        ))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert!(produced
        .iter()
        .any(|link| link.relation == "produces" && link.target.path == fanout.path));

    let reconciled_event = wait_for_watch_event(&observed, |event| {
        let (kind, _, object) = watch_event_parts(event);
        kind == "updated" && object.kind == ObjectKind::Resource && object.path == resource.path
    })
    .await?;
    let fanout_event = wait_for_watch_event(&observed, |event| {
        let (kind, _, object) = watch_event_parts(event);
        kind == "created" && object.kind == ObjectKind::Resource && object.path == fanout.path
    })
    .await?;
    let run_event = wait_for_watch_event(&observed, |event| {
        let (kind, _, object) = watch_event_parts(event);
        kind == "updated"
            && object.kind == ObjectKind::Run
            && object.path == first.path
            && object.value["status"] == "succeeded"
    })
    .await?;
    let produced_event = wait_for_watch_event(&observed, |event| {
        let (kind, _, object) = watch_event_parts(event);
        kind == "created"
            && object.kind == ObjectKind::Link
            && object.value["relation"] == "produces"
    })
    .await?;

    // Exercise exact, `*`, and `**` WatchSelector path matching. The nested
    // child under the one-segment selector and the unrelated path must not be
    // delivered.
    for path in [
        "/watch/exact",
        "/watch/star/one",
        "/watch/star/one/nested",
        "/watch/deep",
        "/watch/deep/a/b",
        "/watch/other",
    ] {
        client
            .post(format!("{api}/resources"))
            .json(&CreateResource {
                path: path.into(),
                manifest_path: note_manifest.path.clone(),
                name: path.rsplit('/').next().unwrap().into(),
                spec: json!({ "archived": false }),
            })
            .send()
            .await?
            .error_for_status()?;
    }
    for expected in [
        "/watch/exact",
        "/watch/star/one",
        "/watch/deep",
        "/watch/deep/a/b",
    ] {
        wait_for_watch_event(&observed, |event| {
            let (kind, _, object) = watch_event_parts(event);
            kind == "created" && object.kind == ObjectKind::Resource && object.path == expected
        })
        .await?;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    let watched_paths: HashSet<String> = observed
        .events
        .lock()
        .unwrap()
        .iter()
        .map(|event| watch_event_parts(event).2.path.clone())
        .collect();
    assert!(!watched_paths.contains("/watch/star/one/nested"));
    assert!(!watched_paths.contains("/watch/other"));

    let mut observed_cursors = [
        watch_event_parts(&reconciled_event).1,
        watch_event_parts(&fanout_event).1,
        watch_event_parts(&run_event).1,
        watch_event_parts(&produced_event).1,
    ];
    observed_cursors.sort_unstable();
    assert!(observed_cursors.windows(2).all(|pair| pair[0] < pair[1]));

    let reconciled: Resource = client
        .get(with_path(
            format!("{api}/resources/by-path"),
            &resource.path,
        ))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(reconciled.status["observed_spec"]["label"], "fixture");

    // A superseding socket disconnects the Runtime. Deleting while it is away
    // verifies that reconnect resumes the watch from its last handled cursor.
    let mut replacement = replace_driver_connection(
        address,
        &driver.path,
        starting.generation,
        &credential.token,
    )
    .await?;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        client
            .delete(with_path(format!("{api}/links/by-path"), &link.path))
            .send()
            .await?
            .status(),
        reqwest::StatusCode::NO_CONTENT
    );
    replacement.close(None).await?;
    let deleted_event = wait_for_watch_event(&observed, |event| {
        let (kind, _, object) = watch_event_parts(event);
        kind == "deleted" && object.kind == ObjectKind::Link && object.path == link.path
    })
    .await?;
    assert!(
        watch_event_parts(&deleted_event).1 > *observed_cursors.last().expect("initial cursor")
    );

    let stopping: DriverRecord = client
        .patch(with_path(format!("{api}/drivers/by-path"), &driver.path))
        .json(&json!({ "state": "stopping" }))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(stopping.state, DriverState::Stopping);
    tokio::time::timeout(Duration::from_secs(5), driver_process).await???;
    let stopped: DriverRecord = client
        .get(with_path(format!("{api}/drivers/by-path"), &driver.path))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(stopped.state, DriverState::Stopped);

    server.abort();
    Ok(())
}
