use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use kas_core::{DriverSpec, DriverState, Resource, RestartPolicy};
use kas_store::Store;
use tokio::{
    process::{Child, Command},
    sync::{mpsc, watch},
    time::{sleep, timeout},
};
use uuid::Uuid;

const READY_TIMEOUT: Duration = Duration::from_secs(15);
const STOP_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_BACKOFF: Duration = Duration::from_secs(30);

fn driver_spec(launch: &DriverLaunch) -> DriverSpec {
    serde_json::from_value(launch.driver.spec.clone())
        .expect("Driver Resource was validated before supervisor launch")
}

fn driver_state(driver: &Resource) -> anyhow::Result<DriverState> {
    serde_json::from_value(serde_json::Value::String(
        driver.status.metadata.state.clone(),
    ))
    .map_err(Into::into)
}

#[derive(Debug, Clone)]
pub(crate) struct DriverLaunch {
    pub manifest_path: String,
    pub package_root: PathBuf,
    pub driver: Resource,
    pub prepared_generation: Option<u64>,
}

#[derive(Clone)]
pub(crate) struct Supervisor {
    commands: mpsc::UnboundedSender<SupervisorCommand>,
}

enum SupervisorCommand {
    EnsureRunning(DriverLaunch),
    Stop(String),
    Finished { driver_path: String, instance: Uuid },
}

impl Supervisor {
    pub fn spawn(store: Arc<Mutex<Store>>, api_url: String, data_dir: PathBuf) -> Self {
        let (commands, receiver) = mpsc::unbounded_channel();
        tokio::spawn(run_supervisor(
            receiver,
            commands.clone(),
            store,
            api_url,
            data_dir,
        ));
        Self { commands }
    }

    pub fn ensure_running(&self, launch: DriverLaunch) -> anyhow::Result<()> {
        self.commands
            .send(SupervisorCommand::EnsureRunning(launch))
            .map_err(|_| anyhow::anyhow!("Driver supervisor is not running"))
    }

    pub fn stop(&self, driver_path: impl Into<String>) -> anyhow::Result<()> {
        self.commands
            .send(SupervisorCommand::Stop(driver_path.into()))
            .map_err(|_| anyhow::anyhow!("Driver supervisor is not running"))
    }
}

async fn run_supervisor(
    mut receiver: mpsc::UnboundedReceiver<SupervisorCommand>,
    commands: mpsc::UnboundedSender<SupervisorCommand>,
    store: Arc<Mutex<Store>>,
    api_url: String,
    data_dir: PathBuf,
) {
    let mut drivers: HashMap<String, (Uuid, watch::Sender<bool>)> = HashMap::new();
    while let Some(command) = receiver.recv().await {
        match command {
            SupervisorCommand::EnsureRunning(launch) => {
                let driver_path = launch.driver.path.clone();
                if drivers
                    .get(&driver_path)
                    .is_some_and(|(_, desired)| *desired.borrow())
                {
                    continue;
                }
                if let Some((_, previous)) = drivers.remove(&driver_path) {
                    let _ = previous.send(false);
                }
                let (desired, desired_receiver) = watch::channel(true);
                let instance = Uuid::new_v4();
                drivers.insert(driver_path.clone(), (instance, desired));
                let completion = commands.clone();
                let task_store = store.clone();
                let task_api_url = api_url.clone();
                let task_data_dir = data_dir.clone();
                tokio::spawn(async move {
                    run_driver(
                        task_store,
                        task_api_url,
                        task_data_dir,
                        launch,
                        desired_receiver,
                    )
                    .await;
                    let _ = completion.send(SupervisorCommand::Finished {
                        driver_path,
                        instance,
                    });
                });
            }
            SupervisorCommand::Stop(driver_path) => {
                if let Some((_, desired)) = drivers.remove(&driver_path) {
                    let _ = desired.send(false);
                } else if let Ok(mut store) = store.lock() {
                    let _ = store.stop_driver(&driver_path);
                }
            }
            SupervisorCommand::Finished {
                driver_path,
                instance,
            } => {
                if drivers
                    .get(&driver_path)
                    .is_some_and(|(current, _)| *current == instance)
                {
                    drivers.remove(&driver_path);
                }
            }
        }
    }
    for (_, (_, desired)) in drivers {
        let _ = desired.send(false);
    }
}

async fn run_driver(
    store: Arc<Mutex<Store>>,
    api_url: String,
    data_dir: PathBuf,
    launch: DriverLaunch,
    mut desired: watch::Receiver<bool>,
) {
    let mut backoff = Duration::from_millis(250);
    loop {
        if !*desired.borrow() {
            return;
        }
        let started =
            match prepare_generation(&store, &launch.driver.path, launch.prepared_generation) {
                Ok(started) => started,
                Err(error) => {
                    eprintln!(
                        "Driver {} could not prepare a generation: {error:#}",
                        launch.driver.path
                    );
                    return;
                }
            };
        let generation = started.0;
        let token = started.1;
        let mut child = match spawn_process(&api_url, &data_dir, &launch, generation, &token) {
            Ok(child) => child,
            Err(error) => {
                mark_failed(&store, &launch.driver.path, generation, &error.to_string());
                if !should_restart(driver_spec(&launch).restart, true) {
                    return;
                }
                if wait_backoff(&mut desired, backoff).await {
                    return;
                }
                backoff = (backoff * 2).min(MAX_BACKOFF);
                continue;
            }
        };

        match wait_until_ready(
            &store,
            &launch.driver.path,
            generation,
            &mut child,
            &mut desired,
        )
        .await
        {
            ReadyOutcome::Ready => backoff = Duration::from_millis(250),
            ReadyOutcome::Stopped => {
                stop_child(&store, &launch.driver.path, generation, &mut child).await;
                return;
            }
            ReadyOutcome::Exited(success) => {
                record_exit(
                    &store,
                    &launch.driver.path,
                    generation,
                    success,
                    "Driver exited before becoming ready",
                );
                if !should_restart(driver_spec(&launch).restart, !success)
                    || wait_backoff(&mut desired, backoff).await
                {
                    return;
                }
                backoff = (backoff * 2).min(MAX_BACKOFF);
                continue;
            }
            ReadyOutcome::TimedOut => {
                let _ = child.kill().await;
                mark_failed(
                    &store,
                    &launch.driver.path,
                    generation,
                    "Driver ready timeout",
                );
                if !should_restart(driver_spec(&launch).restart, true)
                    || wait_backoff(&mut desired, backoff).await
                {
                    return;
                }
                backoff = (backoff * 2).min(MAX_BACKOFF);
                continue;
            }
        }

        tokio::select! {
            status = child.wait() => {
                let success = status.is_ok_and(|status| status.success());
                let failed = !success;
                record_exit(
                    &store,
                    &launch.driver.path,
                    generation,
                    success,
                    "Driver process exited",
                );
                if !should_restart(driver_spec(&launch).restart, failed)
                    || wait_backoff(&mut desired, backoff).await
                {
                    return;
                }
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
            changed = desired.changed() => {
                if changed.is_err() || !*desired.borrow() {
                    stop_child(&store, &launch.driver.path, generation, &mut child).await;
                    return;
                }
            }
        }
    }
}

fn prepare_generation(
    store: &Arc<Mutex<Store>>,
    driver_path: &str,
    prepared_generation: Option<u64>,
) -> anyhow::Result<(u64, String)> {
    let mut store = store
        .lock()
        .map_err(|_| anyhow::anyhow!("Store lock is poisoned"))?;
    let current = store.get_driver(driver_path)?;
    let current_state = driver_state(&current)?;
    let current_generation = store.driver_generation(driver_path)?;
    if prepared_generation.is_some_and(|generation| {
        current_generation == generation && current_state == DriverState::Starting
    }) {
        let credential = store.issue_driver_credential(driver_path)?;
        return Ok((current_generation, credential.token));
    }
    if matches!(
        current_state,
        DriverState::Starting | DriverState::Running | DriverState::Stopping
    ) {
        if current_state != DriverState::Stopping {
            store.stop_driver(driver_path)?;
        }
        store.mark_driver_stopped(driver_path, current_generation)?;
    }
    store.start_driver(driver_path)?;
    let generation = store.driver_generation(driver_path)?;
    let credential = store.issue_driver_credential(driver_path)?;
    Ok((generation, credential.token))
}

fn spawn_process(
    api_url: &str,
    data_dir: &std::path::Path,
    launch: &DriverLaunch,
    generation: u64,
    token: &str,
) -> anyhow::Result<Child> {
    let definition = driver_spec(launch);
    let entrypoint = launch.package_root.join(&definition.entrypoint);
    let mut command = Command::new(&entrypoint);
    command
        .args(&definition.args)
        .current_dir(&launch.package_root)
        .env("KAS_API", api_url)
        .env("KAS_DATA_DIR", data_dir)
        .env("KAS_PACKAGE_ROOT", &launch.package_root)
        .env("KAS_MANIFEST_PATH", &launch.manifest_path)
        .env("KAS_DRIVER_PATH", &launch.driver.path)
        .env("KAS_DRIVER_GENERATION", generation.to_string())
        .env("KAS_DRIVER_TOKEN", token)
        .kill_on_drop(true);
    command.spawn().map_err(Into::into)
}

enum ReadyOutcome {
    Ready,
    Stopped,
    Exited(bool),
    TimedOut,
}

async fn wait_until_ready(
    store: &Arc<Mutex<Store>>,
    driver_path: &str,
    generation: u64,
    child: &mut Child,
    desired: &mut watch::Receiver<bool>,
) -> ReadyOutcome {
    let ready = async {
        loop {
            sleep(Duration::from_millis(50)).await;
            let running = store.lock().ok().is_some_and(|store| {
                store.driver_generation(driver_path).ok() == Some(generation)
                    && store
                        .get_driver(driver_path)
                        .ok()
                        .and_then(|driver| driver_state(&driver).ok())
                        == Some(DriverState::Running)
            });
            if running {
                return ReadyOutcome::Ready;
            }
        }
    };
    tokio::select! {
        outcome = timeout(READY_TIMEOUT, ready) => outcome.unwrap_or(ReadyOutcome::TimedOut),
        status = child.wait() => ReadyOutcome::Exited(status.is_ok_and(|status| status.success())),
        changed = desired.changed() => {
            let _ = changed;
            ReadyOutcome::Stopped
        }
    }
}

async fn stop_child(
    store: &Arc<Mutex<Store>>,
    driver_path: &str,
    generation: u64,
    child: &mut Child,
) {
    if let Ok(mut store) = store.lock() {
        if store
            .get_driver(driver_path)
            .ok()
            .and_then(|driver| driver_state(&driver).ok())
            .is_some_and(|state| matches!(state, DriverState::Starting | DriverState::Running))
        {
            let _ = store.stop_driver(driver_path);
        }
    }
    if timeout(STOP_TIMEOUT, child.wait()).await.is_err() {
        let _ = child.kill().await;
        let _ = child.wait().await;
    }
    if let Ok(mut store) = store.lock() {
        if store
            .get_driver(driver_path)
            .ok()
            .and_then(|driver| driver_state(&driver).ok())
            .is_some_and(|state| state == DriverState::Stopping)
        {
            let _ = store.mark_driver_stopped(driver_path, generation);
        }
    }
}

fn mark_failed(store: &Arc<Mutex<Store>>, driver_path: &str, generation: u64, message: &str) {
    if let Ok(mut store) = store.lock() {
        let _ = store.mark_driver_failed(driver_path, generation, message);
    }
}

fn record_exit(
    store: &Arc<Mutex<Store>>,
    driver_path: &str,
    generation: u64,
    success: bool,
    message: &str,
) {
    if !success {
        mark_failed(store, driver_path, generation, message);
        return;
    }
    if let Ok(mut store) = store.lock() {
        if store
            .get_driver(driver_path)
            .ok()
            .and_then(|driver| driver_state(&driver).ok())
            .is_some_and(|state| matches!(state, DriverState::Starting | DriverState::Running))
        {
            let _ = store.stop_driver(driver_path);
        }
        if store
            .get_driver(driver_path)
            .ok()
            .and_then(|driver| driver_state(&driver).ok())
            .is_some_and(|state| state == DriverState::Stopping)
        {
            let _ = store.mark_driver_stopped(driver_path, generation);
        }
    }
}

fn should_restart(policy: RestartPolicy, failed: bool) -> bool {
    match policy {
        RestartPolicy::Never => false,
        RestartPolicy::OnFailure => failed,
        RestartPolicy::Always => true,
    }
}

async fn wait_backoff(desired: &mut watch::Receiver<bool>, duration: Duration) -> bool {
    tokio::select! {
        _ = sleep(duration) => false,
        changed = desired.changed() => changed.is_err() || !*desired.borrow(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn restart_policy_is_applied_to_exit_status() {
        assert!(!should_restart(RestartPolicy::Never, true));
        assert!(!should_restart(RestartPolicy::Never, false));
        assert!(should_restart(RestartPolicy::OnFailure, true));
        assert!(!should_restart(RestartPolicy::OnFailure, false));
        assert!(should_restart(RestartPolicy::Always, true));
        assert!(should_restart(RestartPolicy::Always, false));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn process_receives_runtime_environment_and_arguments() {
        use std::{fs, os::unix::fs::PermissionsExt};

        let package = tempfile::tempdir().unwrap();
        let entrypoint = package.path().join("driver");
        fs::write(
            &entrypoint,
            "#!/bin/sh\nprintf '%s|%s|%s|%s' \"$KAS_API\" \"$KAS_DRIVER_PATH\" \"$KAS_DRIVER_GENERATION\" \"$1\" > \"$KAS_PACKAGE_ROOT/observed\"\n",
        )
        .unwrap();
        let mut permissions = fs::metadata(&entrypoint).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&entrypoint, permissions).unwrap();
        let launch = DriverLaunch {
            manifest_path: "/manifests/echo".into(),
            package_root: package.path().to_owned(),
            driver: serde_json::from_value(json!({
                "metadata": {
                    "path": "/manifests/echo/driver",
                    "manifest": "/builtin/driver",
                    "name": "driver",
                    "state": "running",
                    "[kas]": {
                        "revision": 0,
                        "observed": {},
                        "created_at": "2026-01-01T00:00:00Z",
                        "updated_at": "2026-01-01T00:00:00Z"
                    }
                },
                "spec": {
                    "runtime": "process",
                    "entrypoint": "./driver",
                    "service_account": "/manifests/echo/service-accounts/driver",
                    "args": ["argument"],
                    "restart": "never"
                },
                "status": {
                    "metadata": {
                        "path": "/manifests/echo/driver",
                        "manifest": "/builtin/driver",
                        "name": "driver",
                        "state": "stopped",
                        "[kas]": {
                            "revision": 0,
                            "observed": {},
                            "created_at": "2026-01-01T00:00:00Z",
                            "updated_at": "2026-01-01T00:00:00Z"
                        }
                    },
                    "spec": {}
                }
            }))
            .unwrap(),
            prepared_generation: None,
        };

        let mut child = spawn_process(
            "http://127.0.0.1:3000",
            package.path(),
            &launch,
            7,
            "secret",
        )
        .unwrap();
        assert!(child.wait().await.unwrap().success());
        assert_eq!(
            fs::read_to_string(package.path().join("observed")).unwrap(),
            "http://127.0.0.1:3000|/manifests/echo/driver|7|argument"
        );
    }
}
