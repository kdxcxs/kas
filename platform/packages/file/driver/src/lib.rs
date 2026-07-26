use std::path::{Path, PathBuf};

use async_trait::async_trait;
use kas_core::{DriverExecution, Mutation, Resource, ResourceStatus};
use kas_driver::{Driver, DriverError};
use serde::{Deserialize, Serialize};
use tokio::fs;
use uuid::Uuid;

pub const FILE_MANIFEST: &str = "/manifests/file";
pub const ATTACHED_TO: &str = "/manifests/file/relations/attached-to";
pub const UPLOADED_BY: &str = "/manifests/file/relations/uploaded-by";

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct FileSpec {
    pub filename: String,
    pub media_type: String,
    pub size: u64,
    pub digest: String,
    pub handle: String,
}

#[derive(Debug, Clone)]
pub struct FileDriver {
    blob_dir: PathBuf,
}

impl FileDriver {
    pub fn new(blob_dir: impl Into<PathBuf>) -> Self {
        Self {
            blob_dir: blob_dir.into(),
        }
    }

    pub fn blob_dir(&self) -> &Path {
        &self.blob_dir
    }

    pub fn blob_path(&self, handle: &str) -> Result<PathBuf, DriverError> {
        Uuid::parse_str(handle)
            .map_err(|_| execution_error(format!("invalid File handle {handle:?}")))?;
        Ok(self.blob_dir.join(handle))
    }
}

#[async_trait]
impl Driver for FileDriver {
    fn name(&self) -> &str {
        "file-content"
    }

    async fn reconcile(&self, resource: &Resource) -> Result<Vec<Mutation>, DriverError> {
        if resource.manifest != FILE_MANIFEST {
            return Err(execution_error(format!(
                "File Driver cannot reconcile Manifest {}",
                resource.manifest
            )));
        }
        let spec: FileSpec = serde_json::from_value(resource.spec.clone())
            .map_err(|error| execution_error(format!("invalid File spec: {error}")))?;
        let blob_path = self.blob_path(&spec.handle)?;
        if resource.metadata.state == kas_core::STATE_DELETED {
            match fs::remove_file(&blob_path).await {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(execution_error(format!(
                        "could not delete File content {}: {error}",
                        blob_path.display()
                    )));
                }
            }
        } else if !fs::try_exists(&blob_path)
            .await
            .map_err(|error| execution_error(error.to_string()))?
        {
            return Err(execution_error(format!(
                "File content {} does not exist",
                blob_path.display()
            )));
        }
        Ok(vec![Mutation::UpdateResourceStatus {
            resource_path: resource.path.clone(),
            expected_revision: resource.revision,
            status: ResourceStatus {
                metadata: resource.status_metadata(resource.metadata.state.clone()),
                spec: resource.spec.clone(),
            },
        }])
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

fn execution_error(message: impl Into<String>) -> DriverError {
    DriverError::Execution(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handles_are_single_uuid_segments() {
        let driver = FileDriver::new("/tmp/files");
        let handle = Uuid::new_v4().to_string();
        assert_eq!(
            driver.blob_path(&handle).unwrap(),
            Path::new("/tmp/files").join(handle)
        );
        assert!(driver.blob_path("../secret").is_err());
    }
}
