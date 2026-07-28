use std::{
    collections::BTreeMap,
    fs,
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::config::{Scenario, ServiceLevelObjectives};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LatencySummary {
    pub count: usize,
    pub min_ms: f64,
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub p99_ms: f64,
    pub max_ms: f64,
    pub mean_ms: f64,
}

impl LatencySummary {
    pub fn from_micros(mut values: Vec<u64>) -> Self {
        if values.is_empty() {
            return Self::default();
        }
        values.sort_unstable();
        let count = values.len();
        let percentile = |fraction: f64| {
            let index = ((count - 1) as f64 * fraction).round() as usize;
            values[index] as f64 / 1000.0
        };
        Self {
            count,
            min_ms: values[0] as f64 / 1000.0,
            p50_ms: percentile(0.50),
            p95_ms: percentile(0.95),
            p99_ms: percentile(0.99),
            max_ms: values[count - 1] as f64 / 1000.0,
            mean_ms: values.iter().sum::<u64>() as f64 / count as f64 / 1000.0,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProcessSummary {
    pub samples: usize,
    pub cpu_mean_percent: f64,
    pub cpu_max_percent: f64,
    pub rss_mean_bytes: u64,
    pub rss_max_bytes: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PhaseResult {
    pub elapsed_ms: f64,
    pub operations: usize,
    pub errors: usize,
    pub throughput_per_second: f64,
    pub latency: LatencySummary,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchmarkResult {
    pub profile: String,
    pub scenario: Scenario,
    pub git_commit: String,
    pub started_at: String,
    pub passed: bool,
    pub failures: Vec<String>,
    pub package_install: PhaseResult,
    pub resource_create: PhaseResult,
    pub convergence: PhaseResult,
    pub steady_get: PhaseResult,
    pub steady_list: PhaseResult,
    pub steady_update: PhaseResult,
    pub api_process: ProcessSummary,
    pub driver_processes: ProcessSummary,
    pub database_bytes: u64,
    pub extra: BTreeMap<String, serde_json::Value>,
}

impl BenchmarkResult {
    pub fn evaluate(&mut self, slo: &ServiceLevelObjectives) {
        let total = self.steady_get.operations
            + self.steady_list.operations
            + self.steady_update.operations;
        let errors = self.steady_get.errors + self.steady_list.errors + self.steady_update.errors;
        let error_rate = if total == 0 {
            1.0
        } else {
            errors as f64 / total as f64
        };
        if error_rate > slo.max_error_rate {
            self.failures.push(format!(
                "error rate {:.4} exceeded {:.4}",
                error_rate, slo.max_error_rate
            ));
        }
        if self.steady_get.latency.p95_ms > slo.get_p95_ms {
            self.failures.push(format!(
                "GET p95 {:.2}ms exceeded {:.2}ms",
                self.steady_get.latency.p95_ms, slo.get_p95_ms
            ));
        }
        if self.steady_update.latency.p95_ms > slo.write_p95_ms {
            self.failures.push(format!(
                "update p95 {:.2}ms exceeded {:.2}ms",
                self.steady_update.latency.p95_ms, slo.write_p95_ms
            ));
        }
        if self.convergence.latency.p95_ms > slo.reconcile_p95_ms {
            self.failures.push(format!(
                "reconcile p95 {:.2}ms exceeded {:.2}ms",
                self.convergence.latency.p95_ms, slo.reconcile_p95_ms
            ));
        }
        if self.convergence.latency.p99_ms > slo.reconcile_p99_ms {
            self.failures.push(format!(
                "reconcile p99 {:.2}ms exceeded {:.2}ms",
                self.convergence.latency.p99_ms, slo.reconcile_p99_ms
            ));
        }
        if self.api_process.cpu_mean_percent > slo.steady_api_cpu_percent {
            self.failures.push(format!(
                "API mean CPU {:.1}% exceeded {:.1}%",
                self.api_process.cpu_mean_percent, slo.steady_api_cpu_percent
            ));
        }
        self.passed = self.failures.is_empty();
    }

    pub fn write(&self, directory: &Path) -> anyhow::Result<()> {
        fs::create_dir_all(directory)?;
        fs::write(
            directory.join("summary.json"),
            serde_json::to_vec_pretty(self)?,
        )?;
        fs::write(directory.join("report.md"), self.markdown())?;
        Ok(())
    }

    fn markdown(&self) -> String {
        format!(
            "# KAS benchmark: {}\n\n\
             - Result: **{}**\n\
             - Resources: {}\n\
             - Manifests: {}\n\
             - Drivers: {}\n\
             - Resource bytes: {}\n\
             - Spec fields/depth: {}/{}\n\
             - Watch fanout: {}\n\
             - Create throughput: {:.1}/s\n\
             - Reconcile p95/p99: {:.2}/{:.2} ms\n\
             - GET p95: {:.2} ms\n\
             - Update p95: {:.2} ms\n\
             - API CPU mean/max: {:.1}/{:.1}%\n\
             - API RSS max: {} bytes\n\
             - Database: {} bytes\n\n\
             ## Failures\n\n{}\n",
            self.profile,
            if self.passed { "PASS" } else { "FAIL" },
            self.scenario.resources,
            self.scenario.manifests,
            self.scenario.drivers,
            self.scenario.resource_bytes,
            self.scenario.spec_fields,
            self.scenario.spec_depth,
            self.scenario.watch_fanout,
            self.resource_create.throughput_per_second,
            self.convergence.latency.p95_ms,
            self.convergence.latency.p99_ms,
            self.steady_get.latency.p95_ms,
            self.steady_update.latency.p95_ms,
            self.api_process.cpu_mean_percent,
            self.api_process.cpu_max_percent,
            self.api_process.rss_max_bytes,
            self.database_bytes,
            if self.failures.is_empty() {
                "None".into()
            } else {
                self.failures
                    .iter()
                    .map(|failure| format!("- {failure}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            }
        )
    }
}

pub fn phase(elapsed: Duration, latencies: Vec<u64>, errors: usize) -> PhaseResult {
    let operations = latencies.len() + errors;
    PhaseResult {
        elapsed_ms: elapsed.as_secs_f64() * 1000.0,
        operations,
        errors,
        throughput_per_second: if elapsed.is_zero() {
            0.0
        } else {
            operations as f64 / elapsed.as_secs_f64()
        },
        latency: LatencySummary::from_micros(latencies),
    }
}

pub fn unix_nanos() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .min(u64::MAX as u128) as u64
}

pub fn git_commit(root: &Path) -> String {
    std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(root)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .unwrap_or_else(|| "unknown".into())
}

pub fn write_samples(path: &Path, samples: &[RequestSample]) -> anyhow::Result<()> {
    let mut body = String::from("phase,operation,path,latency_us,status\n");
    for sample in samples {
        body.push_str(&format!(
            "{},{},{},{},{}\n",
            sample.phase,
            sample.operation,
            sample.path.replace(',', "%2C"),
            sample.latency_us,
            sample.status
        ));
    }
    fs::write(path, body).with_context(|| format!("write {}", path.display()))
}

#[derive(Debug, Clone)]
pub struct RequestSample {
    pub phase: &'static str,
    pub operation: &'static str,
    pub path: String,
    pub latency_us: u64,
    pub status: u16,
}
