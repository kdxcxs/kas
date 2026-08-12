use std::{
    collections::HashSet,
    io::{Cursor, Read},
    path::{Component, Path},
};

use chrono::{DateTime, Utc};
use kas_core::{DriverSpec, PackageDefinition, ResourceDefinition};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

pub const REQUEST_MANIFEST: &str = "/packages/forge/package-request/manifest";
pub const REQUEST_ROOT: &str = "/packages/forge/package-request";
pub const LINK_MANIFEST: &str = "/packages/kas/link/manifest";
pub const USER_MANIFEST: &str = "/packages/kas/user/manifest";
pub const SERVICE_ACCOUNT_MANIFEST: &str = "/packages/kas/service-account/manifest";
pub const REQUESTED_BY_RELATION: &str = "/packages/forge/package-request/relations/requested-by";
pub const DECIDED_BY_RELATION: &str = "/packages/forge/package-request/relations/decided-by";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackageRequestSpec {
    pub digest: String,
    pub size_bytes: u64,
    pub package_path: String,
    pub manifest_path: String,
    pub name: String,
    pub version: u32,
    pub description: String,
    pub resource_count: usize,
    pub has_driver: bool,
    pub reason: String,
    pub submitted_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision: Option<PackageDecision>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackageDecision {
    pub outcome: String,
    pub approver: String,
    pub decided_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PackageInspection {
    pub digest: String,
    pub package_path: String,
    pub manifest_path: String,
    pub name: String,
    pub version: u32,
    pub description: String,
    pub resource_count: usize,
    pub has_driver: bool,
}

pub fn inspect(archive: &[u8]) -> anyhow::Result<PackageInspection> {
    let digest = format!("sha256:{}", hex_digest(archive));
    let mut tar = tar::Archive::new(Cursor::new(archive));
    let mut manifest = None;
    let mut resources = Vec::new();
    let mut files = HashSet::new();
    for entry in tar.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        validate_archive_path(&path)?;
        if !(entry.header().entry_type().is_file() || entry.header().entry_type().is_dir()) {
            anyhow::bail!("Package entries must be regular files or directories");
        }
        if !entry.header().entry_type().is_file() {
            continue;
        }
        if !files.insert(path.clone()) {
            anyhow::bail!("Package contains duplicate file {}", path.display());
        }
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes)?;
        if path == Path::new("manifest.json") {
            manifest = Some(serde_json::from_slice(&bytes)?);
        } else if path.starts_with("resources")
            && path
                .extension()
                .is_some_and(|extension| extension == "json")
        {
            resources.push(serde_json::from_slice::<ResourceDefinition>(&bytes)?);
        } else if !path.starts_with("driver") {
            anyhow::bail!("Unsupported Package file {}", path.display());
        }
    }
    let manifest = manifest.ok_or_else(|| anyhow::anyhow!("Package has no manifest.json"))?;
    let resource_count = resources.len();
    let definition = PackageDefinition {
        manifest,
        resources,
    };
    let name = definition.manifest.name.clone();
    let version = definition.manifest.version;
    let description = definition.manifest.description.clone();
    let expansion = definition.expand(digest.clone())?;
    let mut has_driver = false;
    for resource in &expansion.resources {
        if resource.manifest != "/packages/kas/driver/manifest" {
            continue;
        }
        has_driver = true;
        let driver: DriverSpec = serde_json::from_value(resource.spec.clone())?;
        let entrypoint = driver
            .entrypoint
            .strip_prefix("./")
            .ok_or_else(|| anyhow::anyhow!("Driver entrypoint must be Package-relative"))?;
        if !files.contains(Path::new(entrypoint)) {
            anyhow::bail!("Driver entrypoint does not exist: {entrypoint}");
        }
    }
    let manifest_path = expansion
        .resources
        .first()
        .ok_or_else(|| anyhow::anyhow!("Package expansion is empty"))?
        .path
        .clone();
    Ok(PackageInspection {
        digest,
        package_path: expansion.package_path,
        manifest_path,
        name,
        version,
        description,
        resource_count,
        has_driver,
    })
}

pub fn digest_hex(digest: &str) -> anyhow::Result<&str> {
    let value = digest
        .strip_prefix("sha256:")
        .ok_or_else(|| anyhow::anyhow!("invalid Package digest"))?;
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        anyhow::bail!("invalid Package digest");
    }
    Ok(value)
}

fn validate_archive_path(path: &Path) -> anyhow::Result<()> {
    if path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        anyhow::bail!("Package contains invalid path {}", path.display());
    }
    Ok(())
}

fn hex_digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub fn json_error(error: impl ToString) -> Value {
    serde_json::json!({"error": error.to_string()})
}

#[cfg(test)]
mod tests {
    use super::inspect;
    use serde_json::json;

    #[test]
    fn inspects_a_sandboxed_package() {
        let mut archive = Vec::new();
        {
            let mut tar = tar::Builder::new(&mut archive);
            append(
                &mut tar,
                "manifest.json",
                json!({
                    "path": "/packages/acme/proof/manifest",
                    "manifest": "/packages/kas/manifest/manifest",
                    "name": "proof",
                    "version": 1,
                    "paths": ["./resources/*"],
                    "resource_schema": {"type": "object"},
                    "states": [],
                    "default_state": "available",
                    "initial_state": "available"
                })
                .to_string()
                .as_bytes(),
            );
            tar.finish().unwrap();
        }
        let inspected = inspect(&archive).unwrap();
        assert_eq!(inspected.package_path, "/packages/acme/proof");
        assert_eq!(inspected.resource_count, 0);
        assert!(!inspected.has_driver);
    }

    fn append(builder: &mut tar::Builder<&mut Vec<u8>>, path: &str, bytes: &[u8]) {
        let mut header = tar::Header::new_gnu();
        header.set_path(path).unwrap();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder.append(&header, bytes).unwrap();
    }
}
