use std::{collections::BTreeMap, fs, path::Path};

use anyhow::{bail, Context};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Profile {
    pub name: String,
    pub scenario: Scenario,
    pub slo: ServiceLevelObjectives,
    pub sweeps: BTreeMap<String, Vec<Value>>,
    pub limit: LimitConfig,
}

impl Default for Profile {
    fn default() -> Self {
        Self {
            name: "benchmark".into(),
            scenario: Scenario::default(),
            slo: ServiceLevelObjectives::default(),
            sweeps: BTreeMap::new(),
            limit: LimitConfig::default(),
        }
    }
}

impl Profile {
    pub fn read(path: &Path) -> anyhow::Result<Self> {
        let profile: Self = serde_json::from_slice(
            &fs::read(path).with_context(|| format!("read profile {}", path.display()))?,
        )
        .with_context(|| format!("parse profile {}", path.display()))?;
        profile.validate()?;
        Ok(profile)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        self.scenario.validate()?;
        if self.slo.max_error_rate < 0.0 || self.slo.max_error_rate > 1.0 {
            bail!("slo.max_error_rate must be between 0 and 1");
        }
        if self.limit.repetitions == 0
            || self.limit.required_passes == 0
            || self.limit.required_passes > self.limit.repetitions
        {
            bail!("limit repetitions must be non-zero and include a valid required_passes");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Scenario {
    pub resources: usize,
    pub manifests: usize,
    pub drivers: usize,
    pub resource_bytes: usize,
    pub spec_fields: usize,
    pub spec_depth: usize,
    pub watch_fanout: usize,
    pub write_concurrency: usize,
    pub read_concurrency: usize,
    pub duration_seconds: u64,
    pub reconcile_delay_ms: u64,
    pub get_ratio: u32,
    pub list_ratio: u32,
    pub update_ratio: u32,
    pub convergence_timeout_seconds: u64,
    pub keep_data: bool,
}

impl Default for Scenario {
    fn default() -> Self {
        Self {
            resources: 300,
            manifests: 10,
            drivers: 5,
            resource_bytes: 1024,
            spec_fields: 16,
            spec_depth: 3,
            watch_fanout: 2,
            write_concurrency: 8,
            read_concurrency: 8,
            duration_seconds: 10,
            reconcile_delay_ms: 0,
            get_ratio: 70,
            list_ratio: 10,
            update_ratio: 20,
            convergence_timeout_seconds: 60,
            keep_data: false,
        }
    }
}

impl Scenario {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.resources == 0
            || self.manifests == 0
            || self.spec_fields == 0
            || self.spec_depth == 0
            || self.write_concurrency == 0
            || self.read_concurrency == 0
        {
            bail!("resource, manifest, spec, and concurrency values must be non-zero");
        }
        if self.drivers > self.manifests {
            bail!("drivers must be less than or equal to manifests");
        }
        if self.watch_fanout > self.drivers {
            bail!("watch_fanout must be less than or equal to drivers");
        }
        if self.get_ratio + self.list_ratio + self.update_ratio != 100 {
            bail!("get_ratio + list_ratio + update_ratio must equal 100");
        }
        if self.resource_bytes < 128 {
            bail!("resource_bytes must be at least 128");
        }
        Ok(())
    }

    pub fn set_dimension(&mut self, name: &str, value: &Value) -> anyhow::Result<()> {
        let number = value
            .as_u64()
            .with_context(|| format!("dimension {name} must contain positive integers"))?
            as usize;
        match name {
            "resources" => self.resources = number,
            "manifests" => self.manifests = number,
            "drivers" => self.drivers = number,
            "resource_bytes" => self.resource_bytes = number,
            "spec_fields" => self.spec_fields = number,
            "spec_depth" => self.spec_depth = number,
            "watch_fanout" => self.watch_fanout = number,
            "write_concurrency" => self.write_concurrency = number,
            "read_concurrency" => self.read_concurrency = number,
            "reconcile_delay_ms" => self.reconcile_delay_ms = number as u64,
            other => bail!("unsupported sweep dimension {other}"),
        }
        self.validate()
    }

    pub fn set_dimension_u64(&mut self, name: &str, value: u64) -> anyhow::Result<()> {
        self.set_dimension(name, &Value::from(value))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ServiceLevelObjectives {
    pub max_error_rate: f64,
    pub get_p95_ms: f64,
    pub write_p95_ms: f64,
    pub reconcile_p95_ms: f64,
    pub reconcile_p99_ms: f64,
    pub idle_api_cpu_percent: f64,
    pub steady_api_cpu_percent: f64,
}

impl Default for ServiceLevelObjectives {
    fn default() -> Self {
        Self {
            max_error_rate: 0.0,
            get_p95_ms: 100.0,
            write_p95_ms: 250.0,
            reconcile_p95_ms: 1000.0,
            reconcile_p99_ms: 5000.0,
            idle_api_cpu_percent: 10.0,
            steady_api_cpu_percent: 80.0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LimitConfig {
    pub dimension: String,
    pub start: u64,
    pub max: u64,
    pub multiplier: u64,
    pub repetitions: usize,
    pub required_passes: usize,
}

impl Default for LimitConfig {
    fn default() -> Self {
        Self {
            dimension: "resources".into(),
            start: 1000,
            max: 1_000_000,
            multiplier: 2,
            repetitions: 3,
            required_passes: 2,
        }
    }
}
