use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use kas_api::app;
use kas_auth::{
    CreateRole, CreateRoleBinding, CreateUser, IssuedCredential, Role, RoleBinding, Rule, Subject,
    SubjectKind, User, SYSTEM_ADMIN_ROLE,
};
use kas_core::{
    Action, CreateLink, CreateManifest, CreateResource, CreateRun, DriverState, FinishRun,
    ObjectKind, ObjectRef, RunResult, RunStatus, UpdateResource,
};
use kas_core::{Driver as DriverRecord, Link, Manifest, Resource, Run};
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
    tokio::time::timeout(Duration::from_secs(3), async {
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

async fn replace_driver_connection(
    address: std::net::SocketAddr,
    driver_id: Uuid,
    generation: u64,
    token: &str,
) -> anyhow::Result<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
> {
    let mut request = format!("ws://{address}/drivers/{driver_id}/connect?generation={generation}")
        .into_client_request()?;
    request
        .headers_mut()
        .insert(header::AUTHORIZATION, format!("Bearer {token}").parse()?);
    Ok(tokio_tungstenite::connect_async(request).await?.0)
}

async fn wait_for_finished_run(client: &Client, api: &str, id: Uuid) -> anyhow::Result<Run> {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let run: Run = client
                .get(format!("{api}/runs/{id}"))
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
async fn test_driver_executes_a_run_end_to_end() -> anyhow::Result<()> {
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
    let unauthorized = Client::new().get(format!("{api}/resources")).send().await?;
    assert_eq!(unauthorized.status(), reqwest::StatusCode::UNAUTHORIZED);
    let mut headers = header::HeaderMap::new();
    headers.insert(
        header::AUTHORIZATION,
        format!("Bearer {}", admin.token).parse()?,
    );
    let client = Client::builder().default_headers(headers).build()?;

    let reader: User = client
        .post(format!("{api}/users"))
        .json(&CreateUser {
            name: "reader".into(),
        })
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let reader_role: Role = client
        .post(format!("{api}/roles"))
        .json(&CreateRole {
            name: "resource-reader".into(),
            description: "Can list resources".into(),
            rules: vec![Rule {
                resources: vec!["resources".into()],
                verbs: vec!["list".into()],
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
            name: "reader".into(),
            role_id: reader_role.id,
            subjects: vec![Subject {
                kind: SubjectKind::User,
                id: reader.id,
            }],
        })
        .send()
        .await?
        .error_for_status()?;
    let reader_credential: IssuedCredential = client
        .post(format!("{api}/users/{}/credentials", reader.id))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let reader_client = Client::builder()
        .default_headers({
            let mut headers = header::HeaderMap::new();
            headers.insert(
                header::AUTHORIZATION,
                format!("Bearer {}", reader_credential.token).parse()?,
            );
            headers
        })
        .build()?;
    assert!(reader_client
        .get(format!("{api}/resources"))
        .send()
        .await?
        .status()
        .is_success());
    assert_eq!(
        reader_client
            .post(format!("{api}/manifests"))
            .json(&CreateManifest {
                name: "forbidden".into(),
                version: 1,
                description: "Forbidden".into(),
                resource_schema: json!({ "type": "object" }),
                actions: vec![],
                driver: Some("forbidden-driver".into()),
            })
            .send()
            .await?
            .status(),
        reqwest::StatusCode::FORBIDDEN
    );
    assert_eq!(
        client
            .patch(format!("{api}/roles/{SYSTEM_ADMIN_ROLE}"))
            .json(&CreateRole {
                name: "changed".into(),
                description: "changed".into(),
                rules: vec![]
            })
            .send()
            .await?
            .status(),
        reqwest::StatusCode::CONFLICT
    );
    let bindings: Vec<RoleBinding> = client
        .get(format!("{api}/role-bindings"))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let bootstrap = bindings
        .iter()
        .find(|binding| binding.name == "system:bootstrap-admin")
        .unwrap();
    assert_eq!(
        client
            .delete(format!("{api}/role-bindings/{}", bootstrap.id))
            .send()
            .await?
            .status(),
        reqwest::StatusCode::CONFLICT
    );

    let passive_manifest: Manifest = client
        .post(format!("{api}/manifests"))
        .json(&CreateManifest {
            name: "note".into(),
            version: 1,
            description: "A passive Resource without a Driver".into(),
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
    let passive_driver: Option<DriverRecord> = client
        .get(format!("{api}/manifests/{}/driver", passive_manifest.id))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert!(passive_driver.is_none());
    let passive_resource: Resource = client
        .post(format!("{api}/resources"))
        .json(&CreateResource {
            manifest_id: passive_manifest.id,
            name: "release-note".into(),
            spec: json!({ "archived": false }),
        })
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let updated_passive: Resource = client
        .patch(format!("{api}/resources/{}", passive_resource.id))
        .json(&UpdateResource {
            expected_revision: passive_resource.revision,
            spec: json!({ "archived": true }),
        })
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(updated_passive.revision, passive_resource.revision + 1);
    assert_eq!(updated_passive.spec, json!({ "archived": true }));
    assert_eq!(
        client
            .patch(format!("{api}/resources/{}", passive_resource.id))
            .json(&UpdateResource {
                expected_revision: passive_resource.revision,
                spec: json!({ "archived": false }),
            })
            .send()
            .await?
            .status(),
        reqwest::StatusCode::CONFLICT
    );

    let manifest: Manifest = client
        .post(format!("{api}/manifests"))
        .json(&CreateManifest {
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
        .get(format!("{api}/manifests/{}/driver", manifest.id))
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
        .json::<Vec<kas_auth::ServiceAccount>>()
        .await?
        .into_iter()
        .find(|account| account.driver_id == Some(driver.id))
        .expect("Driver ServiceAccount");
    let fanout_role: Role = client
        .post(format!("{api}/roles"))
        .json(&CreateRole {
            name: "test-driver-fanout".into(),
            description: "Allow the test Driver to atomically create note output".into(),
            rules: vec![
                Rule {
                    resources: vec!["resources/note".into()],
                    verbs: vec!["create".into(), "watch".into()],
                },
                Rule {
                    resources: vec!["links".into()],
                    verbs: vec!["create".into(), "watch".into()],
                },
                Rule {
                    resources: vec!["resources/test".into(), "runs".into()],
                    verbs: vec!["watch".into()],
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
            name: "test-driver-fanout".into(),
            role_id: fanout_role.id,
            subjects: vec![Subject {
                kind: SubjectKind::ServiceAccount,
                id: driver_service_account.id,
            }],
        })
        .send()
        .await?
        .error_for_status()?;
    let starting: DriverRecord = client
        .patch(format!("{api}/drivers/{}", driver.id))
        .json(&json!({ "state": "starting" }))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(starting.state, DriverState::Starting);
    let resource: Resource = client
        .post(format!("{api}/resources"))
        .json(&CreateResource {
            manifest_id: manifest.id,
            name: "fixture".into(),
            spec: json!({
                "label": "fixture",
                "fanout_manifest_id": passive_manifest.id
            }),
        })
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let link: Link = client
        .post(format!("{api}/links"))
        .json(&CreateLink {
            source: ObjectRef {
                kind: ObjectKind::Resource,
                id: passive_resource.id,
            },
            relation: "related_to".into(),
            target: ObjectRef {
                kind: ObjectKind::Resource,
                id: resource.id,
            },
            metadata: json!({ "reason": "e2e" }),
        })
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let links: Vec<Link> = client
        .get(format!(
            "{api}/links?source_kind=resource&source_id={}",
            passive_resource.id
        ))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(links, vec![link.clone()]);
    let fetched_link: Link = client
        .get(format!("{api}/links/{}", link.id))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(fetched_link, link);
    let first: Run = client
        .post(format!("{api}/runs"))
        .json(&CreateRun {
            request_id: Uuid::new_v4(),
            resource_id: resource.id,
            action: "echo".into(),
            input: json!({ "message": "hello" }),
        })
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    let credential: IssuedCredential = client
        .post(format!("{api}/drivers/{}/credentials", driver.id))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let observed = Arc::new(ObservedWatch::default());
    let runtime = DriverRuntime::new(
        &api,
        driver.id,
        starting.generation,
        credential.token.clone(),
        RecordingTestDriver {
            selectors: vec![
                WatchSelector::Resource {
                    manifest_id: Some(manifest.id),
                },
                WatchSelector::Resource {
                    manifest_id: Some(passive_manifest.id),
                },
                WatchSelector::Link {
                    relation: None,
                    source: None,
                    target: None,
                },
                WatchSelector::Run {
                    resource_id: Some(resource.id),
                },
            ],
            observed: observed.clone(),
        },
    )
    .with_reconnect_interval(Duration::from_millis(300));
    let driver_process = tokio::spawn(async move { runtime.run().await });
    let first = wait_for_finished_run(&client, &api, first.id).await?;
    assert_eq!(first.status, RunStatus::Succeeded);
    let first_output = first.output.as_ref().expect("Run output");
    assert_eq!(first_output["echo"], json!({ "message": "hello" }));
    let fanout_resource_id = Uuid::parse_str(
        first_output["fanout_resource_id"]
            .as_str()
            .expect("fanout Resource ID"),
    )?;
    let fanout_resource: Resource = client
        .get(format!("{api}/resources/{fanout_resource_id}"))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(fanout_resource.manifest_id, passive_manifest.id);
    let produced: Vec<Link> = client
        .get(format!(
            "{api}/links?source_kind=run&source_id={}",
            first.id
        ))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert!(produced
        .iter()
        .any(|link| { link.relation == "produces" && link.target.id == fanout_resource_id }));
    let reconciled_event = wait_for_watch_event(&observed, |event| {
        let (kind, _, object) = watch_event_parts(event);
        kind == "updated" && object.kind == ObjectKind::Resource && object.id == resource.id
    })
    .await?;
    let fanout_event = wait_for_watch_event(&observed, |event| {
        let (kind, _, object) = watch_event_parts(event);
        kind == "created" && object.kind == ObjectKind::Resource && object.id == fanout_resource_id
    })
    .await?;
    let run_event = wait_for_watch_event(&observed, |event| {
        let (kind, _, object) = watch_event_parts(event);
        kind == "updated"
            && object.kind == ObjectKind::Run
            && object.id == first.id
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
    let mut observed_cursors = [
        watch_event_parts(&reconciled_event).1,
        watch_event_parts(&fanout_event).1,
        watch_event_parts(&run_event).1,
        watch_event_parts(&produced_event).1,
    ];
    observed_cursors.sort_unstable();
    assert!(observed_cursors.windows(2).all(|pair| pair[0] < pair[1]));
    let repeated: Run = client
        .put(format!("{api}/runs/{}/result", first.id))
        .json(&FinishRun {
            driver_generation: starting.generation,
            result: RunResult::Succeeded {
                output: first.output.clone().unwrap(),
            },
        })
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(repeated, first);

    let reconciled: Resource = client
        .get(format!("{api}/resources/{}", resource.id))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(
        reconciled.status,
        json!({ "observed_spec": {
            "label": "fixture",
            "fanout_manifest_id": passive_manifest.id
        } })
    );

    // Replace the Runtime's socket briefly. The Link is deleted while the
    // Runtime is disconnected, so observing it after reconnect proves that
    // the Runtime resumed its watch from the last successfully handled cursor.
    let mut replacement =
        replace_driver_connection(address, driver.id, starting.generation, &credential.token)
            .await?;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        client
            .delete(format!("{api}/links/{}", link.id))
            .send()
            .await?
            .status(),
        reqwest::StatusCode::NO_CONTENT
    );
    replacement.close(None).await?;
    let deleted_event = wait_for_watch_event(&observed, |event| {
        let (kind, _, object) = watch_event_parts(event);
        kind == "deleted" && object.kind == ObjectKind::Link && object.id == link.id
    })
    .await?;
    assert!(
        watch_event_parts(&deleted_event).1 > *observed_cursors.last().expect("initial cursor")
    );
    assert_eq!(
        client
            .get(format!("{api}/links/{}", link.id))
            .send()
            .await?
            .status(),
        reqwest::StatusCode::NOT_FOUND
    );

    let second: Run = client
        .post(format!("{api}/runs"))
        .json(&CreateRun {
            request_id: Uuid::new_v4(),
            resource_id: resource.id,
            action: "echo".into(),
            input: json!({ "message": "still running" }),
        })
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let second = wait_for_finished_run(&client, &api, second.id).await?;
    assert_eq!(second.status, RunStatus::Succeeded);
    assert_eq!(
        second.output.as_ref().unwrap()["echo"],
        json!({ "message": "still running" })
    );
    wait_for_watch_event(&observed, |event| {
        let (kind, _, object) = watch_event_parts(event);
        kind == "updated"
            && object.kind == ObjectKind::Run
            && object.id == second.id
            && object.value["status"] == "succeeded"
    })
    .await?;
    assert!(!driver_process.is_finished());

    let stopping: DriverRecord = client
        .patch(format!("{api}/drivers/{}", driver.id))
        .json(&json!({ "state": "stopping" }))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(stopping.state, DriverState::Stopping);
    tokio::time::timeout(Duration::from_secs(3), driver_process).await???;
    let stopped: DriverRecord = client
        .get(format!("{api}/drivers/{}", driver.id))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(stopped.state, DriverState::Stopped);

    server.abort();
    Ok(())
}
