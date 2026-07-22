use std::time::Duration;

use kas_api::app;
use kas_auth::{
    CreateRole, CreateRoleBinding, CreateUser, IssuedCredential, Role, RoleBinding, Rule, Subject,
    SubjectKind, User, SYSTEM_ADMIN_ROLE,
};
use kas_core::{
    Action, CreateManifest, CreateResource, CreateRun, DriverState, FinishRun, RunResult, RunStatus,
};
use kas_core::{Driver, Manifest, Resource, Run};
use kas_driver::DriverRuntime;
use kas_store::Store;
use kas_test_driver::TestDriver;
use reqwest::{header, Client};
use serde_json::json;
use uuid::Uuid;

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
                driver: "forbidden-driver".into(),
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
            driver: "test-driver".into(),
        })
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let driver: Driver = client
        .get(format!("{api}/manifests/{}/driver", manifest.id))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let starting: Driver = client
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
            spec: json!({ "label": "fixture" }),
        })
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
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
    let runtime = DriverRuntime::new(
        &api,
        driver.id,
        starting.generation,
        credential.token,
        TestDriver,
    )
    .with_poll_interval(Duration::from_millis(10));
    let driver_process = tokio::spawn(runtime.run());
    let first = wait_for_finished_run(&client, &api, first.id).await?;
    assert_eq!(first.status, RunStatus::Succeeded);
    assert_eq!(
        first.output,
        Some(json!({ "echo": { "message": "hello" } }))
    );
    let repeated: Run = client
        .put(format!("{api}/runs/{}/result", first.id))
        .json(&FinishRun {
            driver_generation: starting.generation,
            result: RunResult::Succeeded {
                output: json!({ "echo": { "message": "hello" } }),
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
        json!({ "observed_spec": { "label": "fixture" } })
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
        second.output,
        Some(json!({ "echo": { "message": "still running" } }))
    );
    assert!(!driver_process.is_finished());

    let stopping: Driver = client
        .patch(format!("{api}/drivers/{}", driver.id))
        .json(&json!({ "state": "stopping" }))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(stopping.state, DriverState::Stopping);
    tokio::time::timeout(Duration::from_secs(3), driver_process).await???;
    let stopped: Driver = client
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
