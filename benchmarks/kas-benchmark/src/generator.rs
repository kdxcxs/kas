use std::{
    io::Cursor,
    path::{Path, PathBuf},
};

use anyhow::Context;
use serde_json::{json, Map, Value};
use tar::{Builder, Header};

use crate::config::Scenario;

pub const MANIFEST_PREFIX: &str = "/benchmark/manifests/m";
pub const RESOURCE_PREFIX: &str = "/benchmark/resources/r";

pub fn manifest_path(index: usize) -> String {
    format!("{MANIFEST_PREFIX}{index:05}")
}

pub fn driver_path(index: usize) -> String {
    format!("{}/driver", manifest_path(index))
}

pub fn resource_path(index: usize) -> String {
    format!("{RESOURCE_PREFIX}{index:09}")
}

pub fn resource_manifest(index: usize, manifests: usize) -> String {
    manifest_path(index % manifests)
}

pub fn create_resource(index: usize, scenario: &Scenario, revision_marker: u64) -> Value {
    let path = resource_path(index);
    let manifest = resource_manifest(index, scenario.manifests);
    let mut spec = generated_spec(scenario.spec_fields, scenario.spec_depth, revision_marker);
    let mut document = json!({
        "path": path,
        "metadata": {
            "manifest": manifest,
            "name": format!("resource-{index:09}")
        },
        "spec": spec
    });
    let current = serde_json::to_vec(&document)
        .map(|bytes| bytes.len())
        .unwrap_or(0);
    if current < scenario.resource_bytes {
        let padding = "x".repeat(scenario.resource_bytes - current);
        spec.as_object_mut()
            .expect("generated spec is an object")
            .insert("padding".into(), Value::String(padding));
        document["spec"] = spec;
    }
    document
}

fn generated_spec(fields: usize, depth: usize, revision_marker: u64) -> Value {
    let mut deepest = Map::new();
    for field in 0..fields {
        deepest.insert(
            format!("field_{field:05}"),
            json!({
                "value": field,
                "marker": revision_marker,
            }),
        );
    }
    let mut value = Value::Object(deepest);
    for level in (1..depth).rev() {
        let mut parent = Map::new();
        parent.insert(format!("level_{level:03}"), value);
        value = Value::Object(parent);
    }
    value
}

pub struct PackageGenerator {
    scenario: Scenario,
    driver_binary: PathBuf,
    metrics_address: String,
}

impl PackageGenerator {
    pub fn new(
        scenario: Scenario,
        driver_binary: impl Into<PathBuf>,
        metrics_address: impl Into<String>,
    ) -> Self {
        Self {
            scenario,
            driver_binary: driver_binary.into(),
            metrics_address: metrics_address.into(),
        }
    }

    pub fn package(&self, manifest_index: usize) -> anyhow::Result<Vec<u8>> {
        let has_driver = manifest_index < self.scenario.drivers;
        let manifest = json!({
            "path": manifest_path(manifest_index),
            "manifest": "/builtin/manifest",
            "name": format!("benchmark-{manifest_index:05}"),
            "version": 1,
            "description": "Generated KAS end-to-end benchmark Manifest",
            "states": [],
            "default_state": "available",
            "initial_state": if has_driver { "pending" } else { "available" },
            "resource_schema": {
                "type": "object"
            }
        });

        let output = Vec::new();
        let mut archive = Builder::new(output);
        append_json(&mut archive, "manifest.json", &manifest)?;
        if has_driver {
            let watches = self.watches_for(manifest_index);
            append_json(
                &mut archive,
                "resources/drivers/driver.json",
                &json!({
                    "path": "./driver",
                    "metadata": {
                        "manifest": "/builtin/driver",
                        "name": format!("benchmark-driver-{manifest_index:05}"),
                        "state": "running"
                    },
                    "spec": {
                        "runtime": "process",
                        "entrypoint": "./driver/kas-benchmark-driver",
                        "service_account": "./service-accounts/driver",
                        "manages": ["."],
                        "args": [
                            "--metrics",
                            self.metrics_address,
                            "--delay-ms",
                            self.scenario.reconcile_delay_ms.to_string()
                        ],
                        "watches": watches,
                        "restart": "never"
                    },
                    "status": {
                        "metadata": {"state": "stopped"},
                        "spec": {}
                    }
                }),
            )?;
            append_json(
                &mut archive,
                "resources/service-accounts/driver.json",
                &json!({
                    "path": "./service-accounts/driver",
                    "metadata": {
                        "manifest": "/builtin/service-account",
                        "name": format!("benchmark-driver-{manifest_index:05}")
                    }
                }),
            )?;
            append_json(
                &mut archive,
                "resources/roles/driver.json",
                &json!({
                    "path": "./roles/driver",
                    "metadata": {
                        "manifest": "/builtin/role",
                        "name": format!("benchmark-driver-{manifest_index:05}")
                    },
                    "spec": {
                        "description": "Benchmark Driver permissions",
                        "rules": []
                    }
                }),
            )?;
            append_json(
                &mut archive,
                "resources/links/driver-role.json",
                &json!({
                    "path": "./links/driver-role",
                    "metadata": {
                        "manifest": "/builtin/link",
                        "name": "benchmark-driver-role"
                    },
                    "spec": {
                        "relation": "/builtin/relations/role-binding",
                        "source": "./service-accounts/driver",
                        "target": "./roles/driver",
                        "metadata": {}
                    }
                }),
            )?;
            let script = format!(
                "#!/bin/sh\nexec {} \"$@\"\n",
                shell_quote(&self.driver_binary)
            );
            append_file(
                &mut archive,
                "driver/kas-benchmark-driver",
                script.as_bytes(),
                0o755,
            )?;
        }
        archive.finish()?;
        archive.into_inner().context("finish generated package")
    }

    fn watches_for(&self, driver_index: usize) -> Vec<Value> {
        if self.scenario.watch_fanout == 0 || self.scenario.drivers == 0 {
            return Vec::new();
        }
        (0..self.scenario.manifests)
            .filter(|manifest_index| {
                (0..self.scenario.watch_fanout)
                    .any(|offset| (manifest_index + offset) % self.scenario.drivers == driver_index)
            })
            .map(|manifest_index| json!({"manifest": manifest_path(manifest_index)}))
            .collect()
    }
}

fn append_json(archive: &mut Builder<Vec<u8>>, path: &str, value: &Value) -> anyhow::Result<()> {
    append_file(archive, path, &serde_json::to_vec_pretty(value)?, 0o644)
}

fn append_file(
    archive: &mut Builder<Vec<u8>>,
    path: &str,
    bytes: &[u8],
    mode: u32,
) -> anyhow::Result<()> {
    let mut header = Header::new_gnu();
    header.set_size(bytes.len() as u64);
    header.set_mode(mode);
    header.set_mtime(0);
    header.set_cksum();
    archive
        .append_data(&mut header, path, Cursor::new(bytes))
        .with_context(|| format!("append {path} to generated package"))
}

fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\"'\"'"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_resource_respects_shape_and_minimum_size() {
        let scenario = Scenario {
            resource_bytes: 2048,
            spec_fields: 7,
            spec_depth: 4,
            ..Scenario::default()
        };
        let resource = create_resource(1, &scenario, 0);
        assert_eq!(resource["path"], resource_path(1));
        assert_eq!(resource["metadata"]["manifest"], manifest_path(1));
        assert!(serde_json::to_vec(&resource).unwrap().len() >= 2048);
        assert!(resource["spec"]["level_001"]["level_002"]["level_003"].is_object());
    }

    #[test]
    fn watch_assignment_reaches_requested_fanout() {
        let scenario = Scenario {
            manifests: 8,
            drivers: 4,
            watch_fanout: 3,
            ..Scenario::default()
        };
        let generator = PackageGenerator::new(scenario, "/tmp/driver", "127.0.0.1:1");
        let total: usize = (0..4)
            .map(|driver| generator.watches_for(driver).len())
            .sum();
        assert_eq!(total, 8 * 3);
    }
}
