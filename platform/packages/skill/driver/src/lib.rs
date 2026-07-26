use std::{
    collections::BTreeSet,
    fs,
    io::{Cursor, Read, Write},
    path::{Component, Path, PathBuf},
};

use async_trait::async_trait;
use kas_core::{
    DriverExecution, LinkSpec, Mutation, PlannedResource, PlannedResourceMetadata, Resource,
    ResourceStatus,
};
use kas_driver::{Driver, DriverError};
use reqwest::blocking::{multipart, Client};
use serde::{Deserialize, Serialize};
use serde_json::json;
use zip::{write::SimpleFileOptions, ZipArchive, ZipWriter};

pub const SKILL_MANIFEST: &str = "/manifests/skill";
pub const FILE_MANIFEST: &str = "/manifests/file";
pub const LINK_MANIFEST: &str = "/builtin/link";
pub const BUNDLE_RELATION: &str = "/manifests/skill/relations/bundle";
pub const OWNS_RELATION: &str = "/manifests/skill/relations/owns";
pub const USES_RELATION: &str = "/manifests/skill/relations/uses";
pub const KAS_SKILL_PATH: &str = "/skills/kas";
pub const KAS_BUNDLE_PATH: &str = "/files/skills/kas/bundles/builtin-v1";
pub const SKILL_MEDIA_TYPE: &str = "application/vnd.kas.skill+zip";

const MAX_ENTRIES: usize = 1024;
const MAX_UNCOMPRESSED_BYTES: u64 = 64 * 1024 * 1024;
const MAX_SKILL_MD_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct SkillSpec {
    pub name: String,
    pub description: String,
    pub allow_implicit_invocation: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedSkill {
    pub spec: SkillSpec,
    pub entries: Vec<PathBuf>,
}

#[derive(Debug, Deserialize)]
struct SkillFrontmatter {
    name: String,
    description: String,
}

#[derive(Debug, Default, Deserialize)]
struct OpenAiMetadata {
    #[serde(default)]
    policy: OpenAiPolicy,
}

#[derive(Debug, Default, Deserialize)]
struct OpenAiPolicy {
    allow_implicit_invocation: Option<bool>,
}

#[derive(Debug, Clone)]
pub struct SkillDriver {
    api: String,
    file_api: String,
    token: String,
}

impl SkillDriver {
    pub fn new(
        api: impl Into<String>,
        file_api: impl Into<String>,
        token: impl Into<String>,
    ) -> Self {
        Self {
            api: api.into().trim_end_matches('/').to_owned(),
            file_api: file_api.into().trim_end_matches('/').to_owned(),
            token: token.into(),
        }
    }

    fn list_resources(&self, manifest: &str) -> Result<Vec<Resource>, DriverError> {
        Client::new()
            .get(format!("{}/resources", self.api))
            .bearer_auth(&self.token)
            .query(&[("manifest", manifest)])
            .send()
            .and_then(reqwest::blocking::Response::error_for_status)
            .and_then(reqwest::blocking::Response::json)
            .map_err(|error| execution_error(format!("could not list {manifest}: {error}")))
    }

    fn fetch_resource(&self, path: &str) -> Result<Option<Resource>, DriverError> {
        let response = Client::new()
            .get(format!("{}/resources/by-path", self.api))
            .bearer_auth(&self.token)
            .query(&[("path", path)])
            .send()
            .map_err(|error| execution_error(format!("could not load {path}: {error}")))?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        response
            .error_for_status()
            .and_then(reqwest::blocking::Response::json)
            .map(Some)
            .map_err(|error| execution_error(format!("could not load {path}: {error}")))
    }

    fn bundle_link(&self, skill_path: &str) -> Result<Option<(Resource, LinkSpec)>, DriverError> {
        let mut matches = self
            .list_resources(LINK_MANIFEST)?
            .into_iter()
            .filter_map(|resource| {
                let link = serde_json::from_value::<LinkSpec>(resource.spec.clone()).ok()?;
                (link.relation == BUNDLE_RELATION && link.source == skill_path)
                    .then_some((resource, link))
            })
            .collect::<Vec<_>>();
        if matches.len() > 1 {
            return Err(execution_error(format!(
                "Skill {skill_path} has more than one bundle Link"
            )));
        }
        Ok(matches.pop())
    }

    fn download_bundle(&self, file_path: &str) -> Result<Vec<u8>, DriverError> {
        Client::new()
            .get(format!("{}/files/content", self.file_api))
            .bearer_auth(&self.token)
            .query(&[("path", file_path)])
            .send()
            .and_then(reqwest::blocking::Response::error_for_status)
            .and_then(reqwest::blocking::Response::bytes)
            .map(|bytes| bytes.to_vec())
            .map_err(|error| {
                execution_error(format!(
                    "could not download Skill bundle {file_path}: {error}"
                ))
            })
    }

    fn ensure_builtin_bundle(&self) -> Result<Resource, DriverError> {
        if let Some(resource) = self.fetch_resource(KAS_BUNDLE_PATH)? {
            return Ok(resource);
        }
        let bundle = builtin_kas_bundle()
            .map_err(|error| execution_error(format!("could not build KAS Skill: {error}")))?;
        Client::new()
            .post(format!("{}/files", self.file_api))
            .bearer_auth(&self.token)
            .query(&[("path", KAS_BUNDLE_PATH)])
            .multipart(
                multipart::Form::new().part(
                    "content",
                    multipart::Part::bytes(bundle)
                        .file_name("kas.skill")
                        .mime_str(SKILL_MEDIA_TYPE)
                        .map_err(|error| execution_error(error.to_string()))?,
                ),
            )
            .send()
            .and_then(reqwest::blocking::Response::error_for_status)
            .and_then(reqwest::blocking::Response::json)
            .map_err(|error| execution_error(format!("could not upload KAS Skill: {error}")))
    }

    fn reconcile_blocking(&self, resource: &Resource) -> Result<Vec<Mutation>, DriverError> {
        if resource.manifest == LINK_MANIFEST {
            let link: LinkSpec = serde_json::from_value(resource.spec.clone())
                .map_err(|error| execution_error(format!("invalid bundle Link: {error}")))?;
            if link.relation != BUNDLE_RELATION {
                return Ok(Vec::new());
            }
            let Some(skill) = self.fetch_resource(&link.source)? else {
                return Ok(Vec::new());
            };
            if skill.manifest != SKILL_MANIFEST {
                return Ok(Vec::new());
            }
            return Ok(vec![Mutation::UpdateResource {
                resource_path: skill.path.clone(),
                expected_revision: skill.revision,
                metadata: None,
                spec: skill.spec,
            }]);
        }
        if resource.manifest != SKILL_MANIFEST {
            return Err(execution_error(format!(
                "Skill Driver cannot reconcile Manifest {}",
                resource.manifest
            )));
        }
        if resource.metadata.state == kas_core::STATE_DELETED {
            return Ok(vec![Mutation::UpdateResourceStatus {
                resource_path: resource.path.clone(),
                expected_revision: resource.revision,
                status: ResourceStatus {
                    metadata: resource.status_metadata(kas_core::STATE_DELETED),
                    spec: resource.spec.clone(),
                },
            }]);
        }
        let expected: SkillSpec = serde_json::from_value(resource.spec.clone())
            .map_err(|error| execution_error(format!("invalid Skill spec: {error}")))?;
        let Some((_, bundle_link)) = self.bundle_link(&resource.path)? else {
            if resource.path != KAS_SKILL_PATH {
                return Ok(Vec::new());
            }
            let bundle = self.ensure_builtin_bundle()?;
            return Ok(vec![Mutation::CreateResource {
                resource: planned_link(
                    format!("{}/links/bundle", resource.path),
                    resource.path.clone(),
                    bundle.path.clone(),
                ),
            }]);
        };
        let file = self
            .fetch_resource(&bundle_link.target)?
            .ok_or_else(|| execution_error("Skill bundle File does not exist"))?;
        if file.manifest != FILE_MANIFEST || file.metadata.state == kas_core::STATE_DELETED {
            return Err(execution_error(format!(
                "Skill bundle {} is not an available File",
                file.path
            )));
        }
        let validated = validate_bundle(&self.download_bundle(&file.path)?)
            .map_err(|error| execution_error(format!("invalid Skill bundle: {error}")))?;
        if validated.spec != expected {
            return Err(execution_error(format!(
                "Skill spec does not match {}/SKILL.md",
                file.path
            )));
        }
        Ok(vec![Mutation::UpdateResourceStatus {
            resource_path: resource.path.clone(),
            expected_revision: resource.revision,
            status: ResourceStatus {
                metadata: resource.status_metadata(kas_core::STATE_AVAILABLE),
                spec: resource.spec.clone(),
            },
        }])
    }
}

#[async_trait]
impl Driver for SkillDriver {
    fn name(&self) -> &str {
        "skill-bundle"
    }

    async fn reconcile(&self, resource: &Resource) -> Result<Vec<Mutation>, DriverError> {
        let driver = self.clone();
        let resource = resource.clone();
        tokio::task::spawn_blocking(move || driver.reconcile_blocking(&resource))
            .await
            .map_err(|error| execution_error(format!("Skill worker failed: {error}")))?
    }

    async fn execute(
        &self,
        _resource: &Resource,
        action: &Resource,
        _run: &Resource,
    ) -> Result<DriverExecution, DriverError> {
        Err(DriverError::UnsupportedAction(action.path.clone()))
    }
}

pub fn validate_bundle(bytes: &[u8]) -> Result<ValidatedSkill, String> {
    let mut archive = ZipArchive::new(Cursor::new(bytes))
        .map_err(|error| format!("bundle is not a readable ZIP: {error}"))?;
    if archive.len() == 0 {
        return Err("bundle is empty".into());
    }
    if archive.len() > MAX_ENTRIES {
        return Err(format!("bundle contains more than {MAX_ENTRIES} entries"));
    }
    let mut paths = BTreeSet::new();
    let mut total = 0_u64;
    let mut skill_markdown = None;
    let mut openai_metadata = None;
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|error| format!("could not read ZIP entry {index}: {error}"))?;
        validate_entry_type(&entry)?;
        let path = safe_entry_path(entry.name())?;
        if !paths.insert(path.clone()) {
            return Err(format!("bundle contains duplicate path {}", path.display()));
        }
        total = total
            .checked_add(entry.size())
            .ok_or_else(|| "bundle uncompressed size overflow".to_owned())?;
        if total > MAX_UNCOMPRESSED_BYTES {
            return Err(format!(
                "bundle expands beyond {MAX_UNCOMPRESSED_BYTES} bytes"
            ));
        }
        if entry.is_dir() {
            continue;
        }
        if path == Path::new("SKILL.md") {
            if entry.size() > MAX_SKILL_MD_BYTES {
                return Err(format!("SKILL.md exceeds {MAX_SKILL_MD_BYTES} bytes"));
            }
            let mut text = String::new();
            entry
                .read_to_string(&mut text)
                .map_err(|error| format!("SKILL.md must be UTF-8: {error}"))?;
            skill_markdown = Some(text);
        } else if path == Path::new("agents/openai.yaml") {
            let mut text = String::new();
            entry
                .read_to_string(&mut text)
                .map_err(|error| format!("agents/openai.yaml must be UTF-8: {error}"))?;
            openai_metadata = Some(text);
        }
    }
    let markdown = skill_markdown.ok_or_else(|| "bundle root must contain SKILL.md".to_owned())?;
    let frontmatter = parse_frontmatter(&markdown)?;
    validate_skill_name(&frontmatter.name)?;
    let description = frontmatter.description.trim().to_owned();
    if description.is_empty() {
        return Err("Skill description cannot be empty".into());
    }
    if description.chars().count() > 1024 {
        return Err("Skill description cannot exceed 1024 characters".into());
    }
    let allow_implicit_invocation = openai_metadata
        .as_deref()
        .map(serde_yaml::from_str::<OpenAiMetadata>)
        .transpose()
        .map_err(|error| format!("invalid agents/openai.yaml: {error}"))?
        .and_then(|metadata| metadata.policy.allow_implicit_invocation)
        .unwrap_or(true);
    Ok(ValidatedSkill {
        spec: SkillSpec {
            name: frontmatter.name,
            description,
            allow_implicit_invocation,
        },
        entries: paths.into_iter().collect(),
    })
}

pub fn extract_bundle(bytes: &[u8], destination: &Path) -> Result<ValidatedSkill, String> {
    let validated = validate_bundle(bytes)?;
    fs::create_dir_all(destination)
        .map_err(|error| format!("could not create {}: {error}", destination.display()))?;
    let mut archive = ZipArchive::new(Cursor::new(bytes))
        .map_err(|error| format!("bundle is not a readable ZIP: {error}"))?;
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|error| format!("could not read ZIP entry {index}: {error}"))?;
        validate_entry_type(&entry)?;
        let relative = safe_entry_path(entry.name())?;
        let output = destination.join(&relative);
        if entry.is_dir() {
            fs::create_dir_all(&output)
                .map_err(|error| format!("could not create {}: {error}", output.display()))?;
            continue;
        }
        if let Some(parent) = output.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| format!("could not create {}: {error}", parent.display()))?;
        }
        let mut file = fs::File::create(&output)
            .map_err(|error| format!("could not create {}: {error}", output.display()))?;
        std::io::copy(&mut entry, &mut file)
            .map_err(|error| format!("could not extract {}: {error}", output.display()))?;
        set_extracted_permissions(&output, entry.unix_mode())?;
    }
    Ok(validated)
}

pub fn builtin_kas_bundle() -> Result<Vec<u8>, String> {
    let mut output = Cursor::new(Vec::new());
    {
        let mut zip = ZipWriter::new(&mut output);
        zip.start_file(
            "SKILL.md",
            SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated)
                .unix_permissions(0o100600),
        )
        .map_err(|error| error.to_string())?;
        zip.write_all(include_bytes!("../../assets/kas/SKILL.md"))
            .map_err(|error| error.to_string())?;
        zip.start_file(
            "agents/openai.yaml",
            SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated)
                .unix_permissions(0o100600),
        )
        .map_err(|error| error.to_string())?;
        zip.write_all(include_bytes!("../../assets/kas/agents/openai.yaml"))
            .map_err(|error| error.to_string())?;
        zip.finish().map_err(|error| error.to_string())?;
    }
    Ok(output.into_inner())
}

fn safe_entry_path(name: &str) -> Result<PathBuf, String> {
    if name.is_empty() || name.contains('\\') || name.contains('\0') {
        return Err(format!("unsafe ZIP entry path {name:?}"));
    }
    let path = Path::new(name);
    if path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(format!("unsafe ZIP entry path {name:?}"));
    }
    Ok(path.to_path_buf())
}

fn validate_entry_type(entry: &zip::read::ZipFile<'_>) -> Result<(), String> {
    let Some(mode) = entry.unix_mode() else {
        return Ok(());
    };
    let kind = mode & 0o170000;
    if kind == 0o120000 {
        return Err(format!(
            "ZIP entry {:?} is a symbolic link; links are forbidden",
            entry.name()
        ));
    }
    if kind != 0 && kind != 0o040000 && kind != 0o100000 {
        return Err(format!(
            "ZIP entry {:?} is not a regular file or directory",
            entry.name()
        ));
    }
    Ok(())
}

fn parse_frontmatter(markdown: &str) -> Result<SkillFrontmatter, String> {
    let normalized = markdown.replace("\r\n", "\n");
    let rest = normalized
        .strip_prefix("---\n")
        .ok_or_else(|| "SKILL.md must start with YAML frontmatter".to_owned())?;
    let (yaml, body) = rest
        .split_once("\n---\n")
        .ok_or_else(|| "SKILL.md frontmatter is not terminated".to_owned())?;
    if body.trim().is_empty() {
        return Err("SKILL.md instructions cannot be empty".into());
    }
    serde_yaml::from_str(yaml).map_err(|error| format!("invalid SKILL.md frontmatter: {error}"))
}

fn validate_skill_name(name: &str) -> Result<(), String> {
    if name.is_empty()
        || name.len() > 64
        || name.starts_with('-')
        || name.ends_with('-')
        || name.contains("--")
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err(format!("invalid Skill name {name:?}"));
    }
    Ok(())
}

#[cfg(unix)]
fn set_extracted_permissions(path: &Path, unix_mode: Option<u32>) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let mode = if unix_mode.is_some_and(|mode| mode & 0o111 != 0) {
        0o700
    } else {
        0o600
    };
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .map_err(|error| format!("could not set permissions on {}: {error}", path.display()))
}

#[cfg(not(unix))]
fn set_extracted_permissions(_path: &Path, _unix_mode: Option<u32>) -> Result<(), String> {
    Ok(())
}

fn planned_link(path: String, source: String, target: String) -> PlannedResource {
    PlannedResource {
        metadata: PlannedResourceMetadata {
            name: path.split('/').next_back().unwrap_or("bundle").to_owned(),
            path,
            manifest: LINK_MANIFEST.into(),
            state: String::new(),
        },
        spec: serde_json::to_value(LinkSpec {
            relation: BUNDLE_RELATION.into(),
            source,
            target,
            metadata: json!({}),
        })
        .expect("Link spec is serializable"),
        status: Default::default(),
    }
}

fn execution_error(message: impl Into<String>) -> DriverError {
    DriverError::Execution(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bundle(entries: &[(&str, &[u8], u32)]) -> Vec<u8> {
        let mut output = Cursor::new(Vec::new());
        {
            let mut zip = ZipWriter::new(&mut output);
            for (name, content, permissions) in entries {
                zip.start_file(
                    *name,
                    SimpleFileOptions::default().unix_permissions(*permissions),
                )
                .unwrap();
                zip.write_all(content).unwrap();
            }
            zip.finish().unwrap();
        }
        output.into_inner()
    }

    #[test]
    fn validates_a_standard_skill_bundle() {
        let bytes = bundle(&[
            (
                "SKILL.md",
                b"---\nname: demo\ndescription: Demo workflow\n---\n\nDo the work.\n",
                0o100600,
            ),
            ("scripts/run.sh", b"#!/bin/sh\n", 0o100700),
        ]);
        let validated = validate_bundle(&bytes).unwrap();
        assert_eq!(validated.spec.name, "demo");
        assert!(validated.spec.allow_implicit_invocation);
        assert_eq!(
            validated.entries,
            vec![PathBuf::from("SKILL.md"), PathBuf::from("scripts/run.sh")]
        );
    }

    #[test]
    fn rejects_symbolic_links() {
        let mut output = Cursor::new(Vec::new());
        {
            let mut zip = ZipWriter::new(&mut output);
            zip.start_file(
                "SKILL.md",
                SimpleFileOptions::default().unix_permissions(0o600),
            )
            .unwrap();
            zip.write_all(b"---\nname: demo\ndescription: Demo workflow\n---\n\nDo it.\n")
                .unwrap();
            zip.add_symlink("scripts/link", "../../secret", SimpleFileOptions::default())
                .unwrap();
            zip.finish().unwrap();
        }
        let bytes = output.into_inner();
        assert!(validate_bundle(&bytes)
            .unwrap_err()
            .contains("symbolic link"));
    }

    #[test]
    fn rejects_missing_or_nested_skill_markdown() {
        let bytes = bundle(&[(
            "demo/SKILL.md",
            b"---\nname: demo\ndescription: Demo\n---\n\nDo it.\n",
            0o100600,
        )]);
        assert_eq!(
            validate_bundle(&bytes).unwrap_err(),
            "bundle root must contain SKILL.md"
        );
    }
}
