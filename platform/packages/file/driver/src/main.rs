use std::{
    env,
    net::SocketAddr,
    path::{Path, PathBuf},
};

use axum::{
    body::Body,
    extract::{DefaultBodyLimit, Multipart, Query, State},
    http::{
        header::{
            ACCEPT_RANGES, AUTHORIZATION, CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_RANGE,
            CONTENT_TYPE, RANGE,
        },
        HeaderMap, HeaderValue, Method, StatusCode,
    },
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use kas_core::{LinkSpec, PlannedResource, PlannedResourceMetadata, Resource};
use kas_driver::DriverRuntime;
use kas_file_driver::{FileDriver, FileSpec, FILE_MANIFEST, UPLOADED_BY};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::{
    fs,
    io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt},
    net::TcpListener,
};
use tokio_util::io::ReaderStream;
use tower_http::cors::{Any, CorsLayer};
use uuid::Uuid;

#[derive(Clone)]
struct FileService {
    api: String,
    driver_token: String,
    blob_dir: PathBuf,
    max_bytes: u64,
    client: Client,
}

#[derive(Debug, Deserialize)]
struct FileQuery {
    path: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ContentQuery {
    path: String,
}

#[derive(Debug, Serialize)]
struct AuthorizationCheck<'a> {
    manifest: &'a str,
    verb: &'a str,
    path: &'a str,
}

#[derive(Debug, Deserialize)]
struct AuthorizationDecision {
    allowed: bool,
    subject: Subject,
}

#[derive(Debug, Deserialize)]
struct Subject {
    path: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let api = env::var("KAS_API").unwrap_or_else(|_| "http://127.0.0.1:3000".into());
    let address: SocketAddr = env::var("KAS_FILE_ADDRESS")
        .unwrap_or_else(|_| "127.0.0.1:3001".into())
        .parse()?;
    let driver_path = env::var("KAS_DRIVER_PATH")?;
    let generation = env::var("KAS_DRIVER_GENERATION")?.parse()?;
    let token = env::var("KAS_DRIVER_TOKEN")?;
    let data_dir = PathBuf::from(
        env::var_os("KAS_DATA_DIR").ok_or_else(|| anyhow::anyhow!("KAS_DATA_DIR is required"))?,
    );
    let blob_dir = data_dir.join("file-driver").join("blobs");
    fs::create_dir_all(&blob_dir).await?;
    let max_bytes = env::var("KAS_FILE_MAX_BYTES")
        .ok()
        .map(|value| value.parse())
        .transpose()?
        .unwrap_or(1024 * 1024 * 1024);
    let listener = TcpListener::bind(address).await?;
    let driver = FileDriver::new(&blob_dir);
    let service = FileService {
        api: api.trim_end_matches('/').to_owned(),
        driver_token: token.clone(),
        blob_dir,
        max_bytes,
        client: Client::new(),
    };
    let app = Router::new()
        .route("/health", get(|| async { Json(json!({"ok": true})) }))
        .route("/files", post(upload_file))
        .route("/files/content", get(download_file).head(head_file))
        .layer(DefaultBodyLimit::max(
            usize::try_from(max_bytes).unwrap_or(usize::MAX),
        ))
        .layer(
            CorsLayer::new()
                .allow_origin(Any)
                .allow_methods([Method::GET, Method::HEAD, Method::POST])
                .allow_headers([AUTHORIZATION, CONTENT_TYPE, RANGE])
                .expose_headers([
                    ACCEPT_RANGES,
                    CONTENT_DISPOSITION,
                    CONTENT_LENGTH,
                    CONTENT_RANGE,
                    CONTENT_TYPE,
                ]),
        )
        .with_state(service);
    let runtime = DriverRuntime::new(api, driver_path, generation, token, driver);
    tokio::select! {
        result = axum::serve(listener, app) => result.map_err(Into::into),
        result = runtime.run() => result,
    }
}

async fn upload_file(
    State(service): State<FileService>,
    headers: HeaderMap,
    Query(query): Query<FileQuery>,
    mut multipart: Multipart,
) -> Result<(StatusCode, Json<Resource>), FileApiError> {
    let path = query
        .path
        .unwrap_or_else(|| format!("/files/{}", Uuid::new_v4()));
    let subject = service.authorize(&headers, "upload", &path).await?;
    let handle = Uuid::new_v4().to_string();
    let temporary_path = service.blob_dir.join(format!(".{handle}.upload"));
    let final_path = service.blob_dir.join(&handle);
    let mut filename = None;
    let mut media_type = None;
    let mut size = 0_u64;
    let mut digest = Sha256::new();
    let mut output = None;
    while let Some(mut field) = multipart.next_field().await.map_err(bad_request)? {
        if field.name() != Some("content") || output.is_some() {
            continue;
        }
        filename = field.file_name().map(str::to_owned);
        media_type = field.content_type().map(str::to_owned);
        let mut file = fs::File::create(&temporary_path).await.map_err(internal)?;
        while let Some(chunk) = field.chunk().await.map_err(bad_request)? {
            size = size
                .checked_add(chunk.len() as u64)
                .ok_or_else(|| bad_request("file size overflow"))?;
            if size > service.max_bytes {
                let _ = fs::remove_file(&temporary_path).await;
                return Err(FileApiError(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    format!("file exceeds {} bytes", service.max_bytes),
                ));
            }
            digest.update(&chunk);
            file.write_all(&chunk).await.map_err(internal)?;
        }
        file.flush().await.map_err(internal)?;
        output = Some(());
    }
    if output.is_none() {
        return Err(bad_request("multipart field content is required"));
    }
    let filename = filename
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "file".into());
    let media_type = media_type.unwrap_or_else(|| "application/octet-stream".into());
    fs::rename(&temporary_path, &final_path)
        .await
        .map_err(internal)?;
    let spec = FileSpec {
        filename: filename.clone(),
        media_type,
        size,
        digest: format!("sha256:{:x}", digest.finalize()),
        handle,
    };
    let resource = PlannedResource {
        metadata: PlannedResourceMetadata {
            path: path.clone(),
            manifest: FILE_MANIFEST.into(),
            name: filename,
            state: kas_core::STATE_AVAILABLE.into(),
        },
        spec: serde_json::to_value(&spec).map_err(internal)?,
        status: Default::default(),
    };
    let created = match service.create_resource(&resource).await {
        Ok(resource) => resource,
        Err(error) => {
            let _ = fs::remove_file(&final_path).await;
            return Err(error);
        }
    };
    if let Err(error) = service
        .create_uploaded_by_link(&created.path, &subject.path)
        .await
    {
        let _ = service.delete_resource(&created).await;
        return Err(error);
    }
    Ok((StatusCode::CREATED, Json(created)))
}

async fn download_file(
    State(service): State<FileService>,
    headers: HeaderMap,
    Query(query): Query<ContentQuery>,
) -> Result<Response, FileApiError> {
    content_response(service, headers, query.path, false).await
}

async fn head_file(
    State(service): State<FileService>,
    headers: HeaderMap,
    Query(query): Query<ContentQuery>,
) -> Result<Response, FileApiError> {
    content_response(service, headers, query.path, true).await
}

async fn content_response(
    service: FileService,
    headers: HeaderMap,
    path: String,
    head: bool,
) -> Result<Response, FileApiError> {
    service.authorize(&headers, "download", &path).await?;
    let resource = service.get_resource(&path).await?;
    if resource.manifest != FILE_MANIFEST || resource.metadata.state == kas_core::STATE_DELETED {
        return Err(not_found("File"));
    }
    let spec: FileSpec = serde_json::from_value(resource.spec).map_err(internal)?;
    let blob_path = blob_path(&service.blob_dir, &spec.handle)?;
    let metadata = fs::metadata(&blob_path).await.map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            not_found("File content")
        } else {
            internal(error)
        }
    })?;
    let total = metadata.len();
    let range = headers
        .get(RANGE)
        .and_then(|value| value.to_str().ok())
        .map(|value| parse_range(value, total))
        .transpose()?;
    let (start, end, status) = range
        .map(|(start, end)| (start, end, StatusCode::PARTIAL_CONTENT))
        .unwrap_or_else(|| (0, total.saturating_sub(1), StatusCode::OK));
    let length = if total == 0 { 0 } else { end - start + 1 };
    let body = if head || length == 0 {
        Body::empty()
    } else {
        let mut file = fs::File::open(&blob_path).await.map_err(internal)?;
        file.seek(std::io::SeekFrom::Start(start))
            .await
            .map_err(internal)?;
        Body::from_stream(ReaderStream::new(file.take(length)))
    };
    let mut response = Response::builder()
        .status(status)
        .header(CONTENT_TYPE, spec.media_type)
        .header(CONTENT_LENGTH, length)
        .header(ACCEPT_RANGES, "bytes")
        .header(CONTENT_DISPOSITION, content_disposition(&spec.filename));
    if status == StatusCode::PARTIAL_CONTENT {
        response = response.header(CONTENT_RANGE, format!("bytes {start}-{end}/{total}"));
    }
    response.body(body).map_err(internal)
}

impl FileService {
    async fn authorize(
        &self,
        headers: &HeaderMap,
        verb: &str,
        path: &str,
    ) -> Result<Subject, FileApiError> {
        let authorization = headers
            .get(AUTHORIZATION)
            .ok_or_else(|| {
                FileApiError(StatusCode::UNAUTHORIZED, "authentication required".into())
            })?
            .clone();
        let response = self
            .client
            .post(format!("{}/auth/check", self.api))
            .header(AUTHORIZATION, authorization)
            .json(&AuthorizationCheck {
                manifest: FILE_MANIFEST,
                verb,
                path,
            })
            .send()
            .await
            .map_err(internal)?;
        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err(FileApiError(
                StatusCode::UNAUTHORIZED,
                "authentication required".into(),
            ));
        }
        let response = response.error_for_status().map_err(upstream)?;
        let decision: AuthorizationDecision = response.json().await.map_err(internal)?;
        if !decision.allowed {
            return Err(FileApiError(
                StatusCode::FORBIDDEN,
                "permission denied".into(),
            ));
        }
        Ok(decision.subject)
    }

    async fn create_resource(&self, resource: &PlannedResource) -> Result<Resource, FileApiError> {
        self.client
            .post(format!("{}/resources", self.api))
            .bearer_auth(&self.driver_token)
            .json(resource)
            .send()
            .await
            .map_err(internal)?
            .error_for_status()
            .map_err(upstream)?
            .json()
            .await
            .map_err(internal)
    }

    async fn create_uploaded_by_link(
        &self,
        file_path: &str,
        subject_path: &str,
    ) -> Result<Resource, FileApiError> {
        let resource = PlannedResource {
            metadata: PlannedResourceMetadata {
                path: format!("{file_path}/links/uploaded-by"),
                manifest: "/builtin/link".into(),
                name: "uploaded-by".into(),
                state: String::new(),
            },
            spec: serde_json::to_value(LinkSpec {
                relation: UPLOADED_BY.into(),
                source: file_path.into(),
                target: subject_path.into(),
                metadata: json!({}),
            })
            .map_err(internal)?,
            status: Default::default(),
        };
        self.create_resource(&resource).await
    }

    async fn get_resource(&self, path: &str) -> Result<Resource, FileApiError> {
        let response = self
            .client
            .get(format!("{}/resources/by-path", self.api))
            .bearer_auth(&self.driver_token)
            .query(&[("path", path)])
            .send()
            .await
            .map_err(internal)?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(not_found("File"));
        }
        response
            .error_for_status()
            .map_err(upstream)?
            .json()
            .await
            .map_err(internal)
    }

    async fn delete_resource(&self, resource: &Resource) -> Result<(), FileApiError> {
        self.client
            .delete(format!("{}/resources/by-path", self.api))
            .bearer_auth(&self.driver_token)
            .query(&[
                ("path", resource.path.as_str()),
                ("expected_revision", &resource.revision.to_string()),
            ])
            .send()
            .await
            .map_err(internal)?
            .error_for_status()
            .map_err(upstream)?;
        Ok(())
    }
}

fn blob_path(root: &Path, handle: &str) -> Result<PathBuf, FileApiError> {
    Uuid::parse_str(handle).map_err(|_| internal(format!("invalid File handle {handle:?}")))?;
    Ok(root.join(handle))
}

fn parse_range(value: &str, total: u64) -> Result<(u64, u64), FileApiError> {
    let range = value
        .strip_prefix("bytes=")
        .and_then(|value| (!value.contains(',')).then_some(value))
        .ok_or_else(|| range_not_satisfiable(total))?;
    let (start, end) = range
        .split_once('-')
        .ok_or_else(|| range_not_satisfiable(total))?;
    if total == 0 {
        return Err(range_not_satisfiable(total));
    }
    if start.is_empty() {
        let suffix = end
            .parse::<u64>()
            .ok()
            .filter(|value| *value > 0)
            .ok_or_else(|| range_not_satisfiable(total))?;
        return Ok((total.saturating_sub(suffix.min(total)), total - 1));
    }
    let start = start
        .parse::<u64>()
        .map_err(|_| range_not_satisfiable(total))?;
    let end = if end.is_empty() {
        total - 1
    } else {
        end.parse::<u64>()
            .map_err(|_| range_not_satisfiable(total))?
            .min(total - 1)
    };
    if start >= total || start > end {
        return Err(range_not_satisfiable(total));
    }
    Ok((start, end))
}

fn content_disposition(filename: &str) -> HeaderValue {
    let ascii = filename
        .chars()
        .map(|character| {
            if character.is_ascii_graphic() && character != '"' && character != '\\' {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    HeaderValue::from_str(&format!("attachment; filename=\"{ascii}\""))
        .unwrap_or_else(|_| HeaderValue::from_static("attachment"))
}

fn range_not_satisfiable(total: u64) -> FileApiError {
    FileApiError(
        StatusCode::RANGE_NOT_SATISFIABLE,
        format!("requested range is not satisfiable for {total} bytes"),
    )
}

fn bad_request(error: impl std::fmt::Display) -> FileApiError {
    FileApiError(StatusCode::BAD_REQUEST, error.to_string())
}

fn not_found(kind: &str) -> FileApiError {
    FileApiError(StatusCode::NOT_FOUND, format!("{kind} not found"))
}

fn upstream(error: reqwest::Error) -> FileApiError {
    let status = error
        .status()
        .and_then(|status| StatusCode::from_u16(status.as_u16()).ok())
        .unwrap_or(StatusCode::BAD_GATEWAY);
    FileApiError(status, error.to_string())
}

fn internal(error: impl std::fmt::Display) -> FileApiError {
    FileApiError(StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
}

#[derive(Debug)]
struct FileApiError(StatusCode, String);

impl IntoResponse for FileApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"error": self.1}))).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_http_byte_ranges() {
        assert_eq!(parse_range("bytes=2-4", 10).unwrap(), (2, 4));
        assert_eq!(parse_range("bytes=7-", 10).unwrap(), (7, 9));
        assert_eq!(parse_range("bytes=-3", 10).unwrap(), (7, 9));
        assert!(parse_range("bytes=10-", 10).is_err());
        assert!(parse_range("items=0-1", 10).is_err());
    }
}
