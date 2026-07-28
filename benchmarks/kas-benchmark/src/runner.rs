use std::{
    collections::{HashMap, HashSet},
    fs::{self, File},
    net::TcpListener,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

use anyhow::{bail, Context};
use chrono::Utc;
use reqwest::{Client, Response, StatusCode};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    net::TcpListener as TokioTcpListener,
    task::JoinHandle,
};

use crate::{
    config::{Profile, Scenario},
    generator::{create_resource, driver_path, manifest_path, resource_path, PackageGenerator},
    metrics::{
        git_commit, phase, unix_nanos, BenchmarkResult, PhaseResult, ProcessSummary, RequestSample,
    },
};

const PACKAGE_MEDIA_TYPE: &str = "application/vnd.kas.manifest+tar";

#[derive(Debug, Clone)]
pub struct BinaryPaths {
    pub api: PathBuf,
    pub migrate: PathBuf,
    pub admin: PathBuf,
    pub benchmark_driver: PathBuf,
}

impl BinaryPaths {
    pub fn from_directory(directory: &Path) -> Self {
        Self {
            api: directory.join("kas-api"),
            migrate: directory.join("kas-migrate"),
            admin: directory.join("kas-admin"),
            benchmark_driver: directory.join("kas-benchmark-driver"),
        }
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        for path in [
            &self.api,
            &self.migrate,
            &self.admin,
            &self.benchmark_driver,
        ] {
            if !path.is_file() {
                bail!(
                    "required binary {} is missing; build kas-api, kas-migrate, kas-admin, \
                     kas-builtin-driver and kas-benchmark --bins first",
                    path.display()
                );
            }
        }
        Ok(())
    }
}

pub struct BenchmarkRunner {
    root: PathBuf,
    binaries: BinaryPaths,
    output_root: PathBuf,
}

impl BenchmarkRunner {
    pub fn new(root: PathBuf, binaries: BinaryPaths, output_root: PathBuf) -> Self {
        Self {
            root,
            binaries,
            output_root,
        }
    }

    pub async fn run(
        &self,
        profile_name: &str,
        scenario: Scenario,
        profile: &Profile,
        run_name: &str,
    ) -> anyhow::Result<(BenchmarkResult, PathBuf)> {
        scenario.validate()?;
        self.binaries.validate()?;
        let started_at = Utc::now().to_rfc3339();
        let run_id = format!(
            "{}-{}-{}",
            Utc::now().format("%Y%m%dT%H%M%S"),
            sanitize(run_name),
            uuid::Uuid::new_v4().simple()
        );
        let result_dir = self.output_root.join(&run_id);
        let data_root = result_dir.join("data");
        let log_root = result_dir.join("logs");
        fs::create_dir_all(&data_root)?;
        fs::create_dir_all(&log_root)?;
        fs::write(
            result_dir.join("config.json"),
            serde_json::to_vec_pretty(&scenario)?,
        )?;
        let sqlite_database = data_root.join("kas.db");
        let database = std::env::var("KAS_BENCHMARK_DATABASE")
            .unwrap_or_else(|_| sqlite_database.to_string_lossy().into_owned());
        let port = reserve_tcp_port()?;
        let api = format!("http://127.0.0.1:{port}");
        let metrics_listener = TokioTcpListener::bind("127.0.0.1:0").await?;
        let metrics_address = metrics_listener.local_addr()?.to_string();
        let driver_metrics = Arc::new(Mutex::new(Vec::<DriverMetric>::new()));
        let metrics_task = receive_driver_metrics(metrics_listener, driver_metrics.clone());

        run_command(&self.binaries.migrate, &[], &database, &data_root, None)
            .context("migrate benchmark database")?;
        let token = run_command(
            &self.binaries.admin,
            &["bootstrap", "benchmark-admin"],
            &database,
            &data_root,
            None,
        )
        .context("bootstrap benchmark administrator")?
        .trim()
        .to_owned();
        let api_log = File::create(log_root.join("kas-api.log"))?;
        let api_error = api_log.try_clone()?;
        let mut api_process = ProcessGuard::spawn(
            Command::new(&self.binaries.api)
                .env("KAS_DATABASE", &database)
                .env("KAS_DATA_DIR", &data_root)
                .env("KAS_ADDRESS", format!("127.0.0.1:{port}"))
                .env("KAS_API_URL", &api)
                .stdout(Stdio::from(api_log))
                .stderr(Stdio::from(api_error)),
        )
        .context("start kas-api")?;
        let api_pid = api_process.id();
        let client = Client::builder()
            .pool_max_idle_per_host(
                scenario
                    .read_concurrency
                    .max(scenario.write_concurrency)
                    .max(8),
            )
            .build()?;
        wait_for_health(&client, &api, Duration::from_secs(15)).await?;

        let package_start = Instant::now();
        let package_generator = PackageGenerator::new(
            scenario.clone(),
            self.binaries.benchmark_driver.canonicalize()?,
            metrics_address,
        );
        let mut package_latencies = Vec::with_capacity(scenario.manifests);
        for manifest in 0..scenario.manifests {
            let bytes = package_generator.package(manifest)?;
            let started = Instant::now();
            let response = client
                .post(format!("{api}/packages"))
                .bearer_auth(&token)
                .header("Content-Type", PACKAGE_MEDIA_TYPE)
                .body(bytes)
                .send()
                .await?;
            let elapsed = started.elapsed().as_micros() as u64;
            if response.status().is_success() {
                package_latencies.push(elapsed);
                let _ = response.bytes().await?;
            } else {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                bail!("install Manifest {manifest} failed with {status}: {body}");
            }
        }
        let package_install = phase(package_start.elapsed(), package_latencies, 0);
        wait_for_drivers(
            &client,
            &api,
            &token,
            scenario.drivers,
            Duration::from_secs(scenario.convergence_timeout_seconds),
        )
        .await?;

        let created_at = Arc::new(Mutex::new(HashMap::<String, u64>::new()));
        let (resource_create, mut samples, actual_resource_bytes) =
            create_resources(&client, &api, &token, &scenario, created_at.clone()).await?;

        let convergence_started = Instant::now();
        let convergence_latencies = wait_for_convergence(
            &scenario,
            &created_at,
            &driver_metrics,
            Duration::from_secs(scenario.convergence_timeout_seconds),
        )
        .await?;
        let convergence = phase(convergence_started.elapsed(), convergence_latencies, 0);

        tokio::time::sleep(Duration::from_millis(300)).await;
        let (idle_api, idle_drivers, _) = sample_processes(api_pid, Duration::from_secs(1)).await;

        let metrics_duration = Duration::from_secs(scenario.duration_seconds.max(1));
        let process_task = tokio::spawn(sample_processes(api_pid, metrics_duration));
        let (steady_get, steady_list, steady_update, steady_samples) =
            steady_workload(&client, &api, &token, &scenario).await?;
        samples.extend(steady_samples);
        let (api_summary, driver_summary, process_rows) = process_task.await?;

        metrics_task.abort();
        let metrics_snapshot = driver_metrics.lock().unwrap().clone();
        let database_bytes = if is_postgres(&database) {
            0
        } else {
            fs::metadata(&database)
                .map(|metadata| metadata.len())
                .unwrap_or(0)
        };
        let mut extra = std::collections::BTreeMap::new();
        extra.insert("actual_resource_bytes".into(), actual_resource_bytes.into());
        extra.insert(
            "idle_api_cpu_percent".into(),
            idle_api.cpu_mean_percent.into(),
        );
        extra.insert(
            "idle_driver_cpu_percent".into(),
            idle_drivers.cpu_mean_percent.into(),
        );
        extra.insert("driver_metrics".into(), metrics_snapshot.len().into());

        let mut result = BenchmarkResult {
            profile: profile_name.into(),
            scenario: scenario.clone(),
            git_commit: git_commit(&self.root),
            started_at,
            passed: false,
            failures: Vec::new(),
            package_install,
            resource_create,
            convergence,
            steady_get,
            steady_list,
            steady_update,
            api_process: api_summary,
            driver_processes: driver_summary,
            database_bytes,
            extra,
        };
        if idle_api.cpu_mean_percent > profile.slo.idle_api_cpu_percent {
            result.failures.push(format!(
                "idle API CPU {:.1}% exceeded {:.1}%",
                idle_api.cpu_mean_percent, profile.slo.idle_api_cpu_percent
            ));
        }
        result.evaluate(&profile.slo);
        crate::metrics::write_samples(&result_dir.join("samples.csv"), &samples)?;
        fs::write(result_dir.join("processes.csv"), process_rows)?;
        result.write(&result_dir)?;
        api_process.stop();
        if !scenario.keep_data {
            let _ = fs::remove_dir_all(&data_root);
        }
        Ok((result, result_dir))
    }
}

#[derive(Debug, Clone, Deserialize)]
struct DriverMetric {
    stage: String,
    driver: String,
    path: String,
    time_ns: u64,
}

fn receive_driver_metrics(
    listener: TokioTcpListener,
    metrics: Arc<Mutex<Vec<DriverMetric>>>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let metrics = metrics.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stream).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    if let Ok(metric) = serde_json::from_str(&line) {
                        metrics.lock().unwrap().push(metric);
                    }
                }
            });
        }
    })
}

async fn create_resources(
    client: &Client,
    api: &str,
    token: &str,
    scenario: &Scenario,
    created_at: Arc<Mutex<HashMap<String, u64>>>,
) -> anyhow::Result<(PhaseResult, Vec<RequestSample>, usize)> {
    let counter = Arc::new(AtomicUsize::new(0));
    let scenario = Arc::new(scenario.clone());
    let started = Instant::now();
    let mut tasks = Vec::new();
    for _ in 0..scenario.write_concurrency {
        let client = client.clone();
        let api = api.to_owned();
        let token = token.to_owned();
        let counter = counter.clone();
        let scenario = scenario.clone();
        let created_at = created_at.clone();
        tasks.push(tokio::spawn(async move {
            let mut latencies = Vec::new();
            let mut samples = Vec::new();
            let mut errors = 0;
            let mut bytes = 0;
            loop {
                let index = counter.fetch_add(1, Ordering::Relaxed);
                if index >= scenario.resources {
                    break;
                }
                let payload = create_resource(index, &scenario, 0);
                bytes += serde_json::to_vec(&payload)
                    .map(|body| body.len())
                    .unwrap_or(0);
                let path = resource_path(index);
                let request_started = Instant::now();
                let response = client
                    .post(format!("{api}/resources"))
                    .bearer_auth(&token)
                    .json(&payload)
                    .send()
                    .await;
                let elapsed = request_started.elapsed().as_micros() as u64;
                let status = response
                    .as_ref()
                    .map(Response::status)
                    .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
                if status.is_success() {
                    created_at
                        .lock()
                        .unwrap()
                        .insert(path.clone(), unix_nanos());
                    latencies.push(elapsed);
                    if let Ok(response) = response {
                        let _ = response.bytes().await;
                    }
                } else {
                    errors += 1;
                }
                samples.push(RequestSample {
                    phase: "create",
                    operation: "create",
                    path,
                    latency_us: elapsed,
                    status: status.as_u16(),
                });
            }
            (latencies, samples, errors, bytes)
        }));
    }
    let mut latencies = Vec::new();
    let mut samples = Vec::new();
    let mut errors = 0;
    let mut bytes = 0;
    for task in tasks {
        let (task_latencies, task_samples, task_errors, task_bytes) = task.await?;
        latencies.extend(task_latencies);
        samples.extend(task_samples);
        errors += task_errors;
        bytes += task_bytes;
    }
    let actual_bytes = bytes / scenario.resources.max(1);
    Ok((
        phase(started.elapsed(), latencies, errors),
        samples,
        actual_bytes,
    ))
}

async fn wait_for_convergence(
    scenario: &Scenario,
    created_at: &Arc<Mutex<HashMap<String, u64>>>,
    driver_metrics: &Arc<Mutex<Vec<DriverMetric>>>,
    timeout: Duration,
) -> anyhow::Result<Vec<u64>> {
    if scenario.drivers == 0 {
        return Ok(Vec::new());
    }
    let expected: HashSet<(String, String)> = (0..scenario.resources)
        .filter_map(|index| {
            let manifest = index % scenario.manifests;
            (manifest < scenario.drivers).then(|| (driver_path(manifest), resource_path(index)))
        })
        .collect();
    let deadline = Instant::now() + timeout;
    loop {
        let snapshot = driver_metrics.lock().unwrap().clone();
        let completed: HashMap<(String, String), u64> = snapshot
            .iter()
            .filter(|metric| metric.stage == "completed")
            .map(|metric| ((metric.driver.clone(), metric.path.clone()), metric.time_ns))
            .collect();
        if expected.iter().all(|key| completed.contains_key(key)) {
            let creates = created_at.lock().unwrap();
            return Ok(expected
                .iter()
                .filter_map(|key| {
                    let finished = completed.get(key)?;
                    let created = creates.get(&key.1)?;
                    Some(finished.saturating_sub(*created) / 1000)
                })
                .collect());
        }
        if Instant::now() >= deadline {
            let missing = expected
                .iter()
                .filter(|key| !completed.contains_key(*key))
                .count();
            bail!("{missing} owned Resources did not converge before timeout");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn steady_workload(
    client: &Client,
    api: &str,
    token: &str,
    scenario: &Scenario,
) -> anyhow::Result<(PhaseResult, PhaseResult, PhaseResult, Vec<RequestSample>)> {
    let workers = scenario.read_concurrency.max(scenario.write_concurrency);
    let deadline = Instant::now() + Duration::from_secs(scenario.duration_seconds.max(1));
    let sequence = Arc::new(AtomicUsize::new(0));
    let update_sequence = Arc::new(AtomicUsize::new(0));
    let managed_resources = Arc::new(
        (0..scenario.resources)
            .filter(|index| index % scenario.manifests < scenario.drivers)
            .collect::<Vec<_>>(),
    );
    let scenario = Arc::new(scenario.clone());
    let started = Instant::now();
    let mut tasks = Vec::new();
    for worker in 0..workers {
        let client = client.clone();
        let api = api.to_owned();
        let token = token.to_owned();
        let sequence = sequence.clone();
        let update_sequence = update_sequence.clone();
        let managed_resources = managed_resources.clone();
        let scenario = scenario.clone();
        tasks.push(tokio::spawn(async move {
            let mut samples = Vec::new();
            while Instant::now() < deadline {
                let operation = sequence.fetch_add(1, Ordering::Relaxed);
                let bucket = (operation % 100) as u32;
                if bucket < scenario.get_ratio {
                    let index = operation % scenario.resources;
                    let path = resource_path(index);
                    samples.push(measured_get(&client, &api, &token, "get", path, None).await);
                } else if bucket < scenario.get_ratio + scenario.list_ratio {
                    let manifest = manifest_path(operation % scenario.manifests);
                    samples.push(
                        measured_get(
                            &client,
                            &api,
                            &token,
                            "list",
                            manifest.clone(),
                            Some(manifest),
                        )
                        .await,
                    );
                } else if let Some(index) = managed_resources
                    .get(update_sequence.fetch_add(1, Ordering::Relaxed))
                    .copied()
                {
                    samples.push(
                        measured_update(&client, &api, &token, index, &scenario, operation as u64)
                            .await,
                    );
                } else {
                    let index = (operation + worker) % scenario.resources;
                    samples.push(
                        measured_get(&client, &api, &token, "get", resource_path(index), None)
                            .await,
                    );
                }
            }
            samples
        }));
    }
    let mut samples = Vec::new();
    for task in tasks {
        samples.extend(task.await?);
    }
    let elapsed = started.elapsed();
    let summarize = |operation: &str| {
        let selected: Vec<_> = samples
            .iter()
            .filter(|sample| sample.operation == operation)
            .collect();
        let errors = selected
            .iter()
            .filter(|sample| !(200..300).contains(&sample.status))
            .count();
        let latencies = selected
            .iter()
            .filter(|sample| (200..300).contains(&sample.status))
            .map(|sample| sample.latency_us)
            .collect();
        phase(elapsed, latencies, errors)
    };
    Ok((
        summarize("get"),
        summarize("list"),
        summarize("update"),
        samples,
    ))
}

async fn measured_get(
    client: &Client,
    api: &str,
    token: &str,
    operation: &'static str,
    path: String,
    manifest: Option<String>,
) -> RequestSample {
    let started = Instant::now();
    let request = if let Some(manifest) = manifest {
        client
            .get(with_query(api, "/resources", "manifest", &manifest))
            .bearer_auth(token)
    } else {
        client
            .get(with_query(api, "/resources/by-path", "path", &path))
            .bearer_auth(token)
    };
    let response = request.send().await;
    let latency_us = started.elapsed().as_micros() as u64;
    let status = response
        .as_ref()
        .map(Response::status)
        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    if let Ok(response) = response {
        let _ = response.bytes().await;
    }
    RequestSample {
        phase: "steady",
        operation,
        path,
        latency_us,
        status: status.as_u16(),
    }
}

async fn measured_update(
    client: &Client,
    api: &str,
    token: &str,
    index: usize,
    scenario: &Scenario,
    marker: u64,
) -> RequestSample {
    let path = resource_path(index);
    let started = Instant::now();
    let current = client
        .get(with_query(api, "/resources/by-path", "path", &path))
        .bearer_auth(token)
        .send()
        .await;
    let response = match current {
        Ok(current) if current.status().is_success() => match current.json::<Value>().await {
            Ok(current) => {
                let revision = current["metadata"]["[kas]"]["revision"]
                    .as_u64()
                    .unwrap_or(0);
                let spec = create_resource(index, scenario, marker)["spec"].clone();
                client
                    .patch(with_query(api, "/resources/by-path", "path", &path))
                    .bearer_auth(token)
                    .json(&json!({"expected_revision": revision, "spec": spec}))
                    .send()
                    .await
            }
            Err(error) => Err(error),
        },
        Ok(current) => {
            let status = current.status();
            let latency_us = started.elapsed().as_micros() as u64;
            return RequestSample {
                phase: "steady",
                operation: "update",
                path,
                latency_us,
                status: status.as_u16(),
            };
        }
        Err(error) => Err(error),
    };
    let latency_us = started.elapsed().as_micros() as u64;
    let status = response
        .as_ref()
        .map(Response::status)
        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    if let Ok(response) = response {
        let _ = response.bytes().await;
    }
    RequestSample {
        phase: "steady",
        operation: "update",
        path,
        latency_us,
        status: status.as_u16(),
    }
}

async fn wait_for_health(client: &Client, api: &str, timeout: Duration) -> anyhow::Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if client
            .get(format!("{api}/health"))
            .send()
            .await
            .is_ok_and(|response| response.status().is_success())
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("kas-api did not become healthy before timeout");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn wait_for_drivers(
    client: &Client,
    api: &str,
    token: &str,
    expected: usize,
    timeout: Duration,
) -> anyhow::Result<()> {
    if expected == 0 {
        return Ok(());
    }
    let deadline = Instant::now() + timeout;
    loop {
        let response = client
            .get(with_query(api, "/resources", "manifest", "/builtin/driver"))
            .bearer_auth(token)
            .send()
            .await?;
        if response.status().is_success() {
            let resources: Vec<Value> = response.json().await?;
            let running = resources
                .iter()
                .filter(|resource| {
                    resource["path"]
                        .as_str()
                        .is_some_and(|path| path.starts_with("/benchmark/manifests/"))
                        && resource["status"]["metadata"]["state"] == "running"
                })
                .count();
            if running == expected {
                return Ok(());
            }
        }
        if Instant::now() >= deadline {
            bail!("{expected} benchmark Drivers did not become ready before timeout");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn sample_processes(
    api_pid: u32,
    duration: Duration,
) -> (ProcessSummary, ProcessSummary, String) {
    let deadline = Instant::now() + duration;
    let mut api = Vec::<(f64, u64)>::new();
    let mut drivers = Vec::<(f64, u64)>::new();
    let mut csv = String::from("elapsed_ms,kind,cpu_percent,rss_bytes\n");
    let started = Instant::now();
    while Instant::now() < deadline {
        if let Some((api_sample, driver_sample)) = process_snapshot(api_pid).await {
            let elapsed = started.elapsed().as_millis();
            csv.push_str(&format!(
                "{elapsed},api,{:.3},{}\n{elapsed},drivers,{:.3},{}\n",
                api_sample.0, api_sample.1, driver_sample.0, driver_sample.1
            ));
            api.push(api_sample);
            drivers.push(driver_sample);
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    (summarize_process(&api), summarize_process(&drivers), csv)
}

async fn process_snapshot(api_pid: u32) -> Option<((f64, u64), (f64, u64))> {
    let output = tokio::process::Command::new("ps")
        .args(["-axo", "pid=,ppid=,%cpu=,rss="])
        .output()
        .await
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let mut rows = Vec::new();
    for line in text.lines() {
        let fields: Vec<_> = line.split_whitespace().collect();
        if fields.len() != 4 {
            continue;
        }
        rows.push((
            fields[0].parse::<u32>().ok()?,
            fields[1].parse::<u32>().ok()?,
            fields[2].parse::<f64>().ok()?,
            fields[3].parse::<u64>().ok()? * 1024,
        ));
    }
    let mut descendants = HashSet::from([api_pid]);
    loop {
        let before = descendants.len();
        for (pid, ppid, _, _) in &rows {
            if descendants.contains(ppid) {
                descendants.insert(*pid);
            }
        }
        if descendants.len() == before {
            break;
        }
    }
    let api = rows
        .iter()
        .find(|(pid, _, _, _)| *pid == api_pid)
        .map(|(_, _, cpu, rss)| (*cpu, *rss))?;
    let drivers = rows
        .iter()
        .filter(|(pid, _, _, _)| *pid != api_pid && descendants.contains(pid))
        .fold((0.0, 0_u64), |total, (_, _, cpu, rss)| {
            (total.0 + cpu, total.1 + rss)
        });
    Some((api, drivers))
}

fn summarize_process(samples: &[(f64, u64)]) -> ProcessSummary {
    if samples.is_empty() {
        return ProcessSummary::default();
    }
    ProcessSummary {
        samples: samples.len(),
        cpu_mean_percent: samples.iter().map(|sample| sample.0).sum::<f64>() / samples.len() as f64,
        cpu_max_percent: samples.iter().map(|sample| sample.0).fold(0.0, f64::max),
        rss_mean_bytes: samples.iter().map(|sample| sample.1).sum::<u64>() / samples.len() as u64,
        rss_max_bytes: samples.iter().map(|sample| sample.1).max().unwrap_or(0),
    }
}

fn run_command(
    program: &Path,
    arguments: &[&str],
    database: &str,
    data_root: &Path,
    api: Option<&str>,
) -> anyhow::Result<String> {
    let mut command = Command::new(program);
    command
        .args(arguments)
        .env("KAS_DATABASE", database)
        .env("KAS_DATA_DIR", data_root);
    if let Some(api) = api {
        command.env("KAS_API_URL", api);
    }
    let output = command.output()?;
    if !output.status.success() {
        bail!(
            "{} failed: {}",
            program.display(),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn is_postgres(database: &str) -> bool {
    database.starts_with("postgres://") || database.starts_with("postgresql://")
}

fn reserve_tcp_port() -> anyhow::Result<u16> {
    Ok(TcpListener::bind("127.0.0.1:0")?.local_addr()?.port())
}

fn with_query(api: &str, endpoint: &str, key: &str, value: &str) -> String {
    let mut url = url::Url::parse(&format!("{}{}", api.trim_end_matches('/'), endpoint))
        .expect("benchmark API URL is valid");
    url.query_pairs_mut().append_pair(key, value);
    url.into()
}

fn sanitize(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '-' {
                character
            } else {
                '-'
            }
        })
        .collect()
}

struct ProcessGuard {
    child: Child,
}

impl ProcessGuard {
    fn spawn(command: &mut Command) -> anyhow::Result<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        Ok(Self {
            child: command.spawn()?,
        })
    }

    fn id(&self) -> u32 {
        self.child.id()
    }

    fn stop(&mut self) {
        #[cfg(unix)]
        {
            let group = format!("-{}", self.child.id());
            let _ = Command::new("kill")
                .args(["-TERM", &group])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            for _ in 0..20 {
                if self.child.try_wait().ok().flatten().is_some() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            let _ = Command::new("kill")
                .args(["-KILL", &group])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
        #[cfg(not(unix))]
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for ProcessGuard {
    fn drop(&mut self) {
        self.stop();
    }
}
