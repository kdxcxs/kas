use std::{
    collections::HashSet,
    fs,
    io::{Cursor, Read},
    path::{Component, Path, PathBuf},
};

use kas_core::{CreateManifest, ManifestDefinition};
use sha2::{Digest, Sha256};
use uuid::Uuid;

const MANIFEST_FILE: &str = "manifest.json";

#[derive(Debug, Clone)]
pub(crate) struct InstalledPackage {
    pub size_bytes: u64,
    pub root: PathBuf,
    pub manifest: CreateManifest,
}

pub(crate) fn inspect(archive: &[u8]) -> anyhow::Result<CreateManifest> {
    let digest = format!("sha256:{}", hex_digest(archive));
    let mut tar = tar::Archive::new(Cursor::new(archive));
    let mut manifest_json = None;
    let mut files = HashSet::new();
    for entry in tar.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        validate_archive_path(&path)?;
        if !(entry.header().entry_type().is_file() || entry.header().entry_type().is_dir()) {
            anyhow::bail!(
                "package entry must be a regular file or directory: {}",
                path.display()
            );
        }
        if entry.header().entry_type().is_file() {
            files.insert(path.clone());
            if path == Path::new(MANIFEST_FILE) {
                let mut bytes = Vec::new();
                entry.read_to_end(&mut bytes)?;
                manifest_json = Some(bytes);
            }
        }
    }
    let definition: ManifestDefinition = serde_json::from_slice(
        manifest_json
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("package must contain manifest.json at its root"))?,
    )?;
    let manifest = definition.resolve(digest)?;
    if let Some(driver) = &manifest.driver {
        let relative = normalized_package_relative("driver entrypoint", &driver.entrypoint)?;
        if !files.contains(Path::new(&relative)) {
            anyhow::bail!("driver entrypoint does not exist: {relative}");
        }
    }
    Ok(manifest)
}

pub(crate) fn install(data_dir: &Path, archive: &[u8]) -> anyhow::Result<InstalledPackage> {
    let hex = hex_digest(archive);
    let packages = data_dir.join("packages");
    let final_root = packages.join("sha256").join(&hex);
    let staging_root = packages
        .join(".staging")
        .join(format!("{hex}-{}", Uuid::new_v4()));
    fs::create_dir_all(staging_root.parent().expect("staging has a parent"))?;
    fs::create_dir_all(final_root.parent().expect("package has a parent"))?;

    let result = (|| {
        let manifest = inspect(archive)?;
        unpack(archive, &staging_root)?;

        if final_root.exists() {
            fs::remove_dir_all(&staging_root)?;
        } else {
            match fs::rename(&staging_root, &final_root) {
                Ok(()) => {}
                Err(_) if final_root.exists() => fs::remove_dir_all(&staging_root)?,
                Err(error) => return Err(error.into()),
            }
        }
        Ok(InstalledPackage {
            size_bytes: archive.len() as u64,
            root: final_root,
            manifest,
        })
    })();

    if result.is_err() && staging_root.exists() {
        let _ = fs::remove_dir_all(&staging_root);
    }
    result
}

fn unpack(archive: &[u8], destination: &Path) -> anyhow::Result<()> {
    fs::create_dir_all(destination)?;
    let mut tar = tar::Archive::new(Cursor::new(archive));
    for entry in tar.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        validate_archive_path(&path)?;
        if !(entry.header().entry_type().is_file() || entry.header().entry_type().is_dir()) {
            anyhow::bail!(
                "package entry must be a regular file or directory: {}",
                path.display()
            );
        }
        entry.unpack_in(destination)?;
    }
    if !destination.join(MANIFEST_FILE).is_file() {
        anyhow::bail!("package must contain manifest.json at its root");
    }
    Ok(())
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
        anyhow::bail!("package contains an invalid path: {}", path.display());
    }
    Ok(())
}

fn normalized_package_relative(kind: &str, path: &str) -> anyhow::Result<String> {
    let Some(relative) = path.strip_prefix("./") else {
        anyhow::bail!("{kind} must start with ./");
    };
    if relative.is_empty()
        || relative
            .split('/')
            .any(|segment| segment.is_empty() || segment == "." || segment == "..")
        || Path::new(relative).is_absolute()
    {
        anyhow::bail!("{kind} must be a normalized package-relative path");
    }
    Ok(relative.to_owned())
}

fn hex_digest(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn installs_and_resolves_relative_paths() {
        let manifest = serde_json::json!({
            "path": "/manifests/echo",
            "name": "echo",
            "version": 1,
            "description": "Echo",
            "resource_schema": {"type": "object"},
            "actions": [{
                "path": "./actions/echo",
                "name": "echo",
                "description": "Echo",
                "input_schema": {},
                "output_schema": {}
            }],
            "relations": [],
            "driver": {
                "path": "./driver",
                "runtime": "process",
                "entrypoint": "./bin/driver",
                "service_account": "./service-accounts/driver",
                "args": [],
                "restart": "on_failure"
            },
            "rbac": {
                "service_accounts": [{
                    "path": "./service-accounts/driver",
                    "name": "driver"
                }],
                "roles": [],
                "role_bindings": []
            }
        });
        let mut bytes = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut bytes);
            append(
                &mut builder,
                "manifest.json",
                manifest.to_string().as_bytes(),
                0o644,
            );
            append(&mut builder, "bin/driver", b"driver", 0o755);
            builder.finish().unwrap();
        }
        let data = tempfile::tempdir().unwrap();
        let installed = install(data.path(), &bytes).unwrap();
        assert!(installed.manifest.package_digest.starts_with("sha256:"));
        assert_eq!(installed.size_bytes, bytes.len() as u64);
        assert_eq!(
            installed.manifest.actions[0].path,
            "/manifests/echo/actions/echo"
        );
        let driver = installed.manifest.driver.unwrap();
        assert_eq!(driver.path, "/manifests/echo/driver");
        assert_eq!(driver.entrypoint, "./bin/driver");
        assert!(installed.root.join("bin/driver").is_file());
        let repeated = install(data.path(), &bytes).unwrap();
        assert_eq!(repeated.root, installed.root);
        assert_eq!(
            repeated.manifest.package_digest,
            installed.manifest.package_digest
        );
    }

    #[test]
    fn rejects_non_normalized_member_paths() {
        assert!(normalized_package_relative("action", "actions/echo").is_err());
        assert!(normalized_package_relative("action", "./actions/../echo").is_err());
        assert!(normalized_package_relative("action", "./actions//echo").is_err());
    }

    fn append(builder: &mut tar::Builder<&mut Vec<u8>>, path: &str, bytes: &[u8], mode: u32) {
        let mut header = tar::Header::new_gnu();
        header.set_path(path).unwrap();
        header.set_size(bytes.len() as u64);
        header.set_mode(mode);
        header.set_cksum();
        builder.append(&header, bytes).unwrap();
        builder.get_mut().flush().unwrap();
    }
}
