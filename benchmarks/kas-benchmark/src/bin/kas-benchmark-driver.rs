use std::{collections::HashMap, env, process, time::Duration};

use anyhow::{bail, Context};
use futures_util::{SinkExt, StreamExt};
use kas_core::{Mutation, Resource, ResourceStatus};
use kas_driver::{ClientMessage, MutationStatus, ServerMessage};
use serde::Serialize;
use serde_json::json;
use tokio::{io::AsyncWriteExt, net::TcpStream};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{client::IntoClientRequest, http::HeaderValue, Message},
};
use uuid::Uuid;

#[derive(Serialize)]
struct DriverMetric<'a> {
    stage: &'a str,
    driver: &'a str,
    path: &'a str,
    time_ns: u64,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut metrics_address = None;
    let mut delay_ms = 0_u64;
    let mut arguments = env::args().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--metrics" => metrics_address = arguments.next(),
            "--delay-ms" => {
                delay_ms = arguments
                    .next()
                    .context("--delay-ms requires a value")?
                    .parse()
                    .context("invalid --delay-ms")?;
            }
            other => bail!("unknown benchmark Driver argument {other}"),
        }
    }
    let metrics_address = metrics_address.context("--metrics is required")?;
    let mut metrics = TcpStream::connect(&metrics_address).await?;

    let api = env::var("KAS_API").context("KAS_API is required")?;
    let driver_path = env::var("KAS_DRIVER_PATH").context("KAS_DRIVER_PATH is required")?;
    let generation: u64 = env::var("KAS_DRIVER_GENERATION")
        .context("KAS_DRIVER_GENERATION is required")?
        .parse()?;
    let token = env::var("KAS_DRIVER_TOKEN").context("KAS_DRIVER_TOKEN is required")?;
    let websocket_api = api
        .strip_prefix("http://")
        .map(|rest| format!("ws://{rest}"))
        .or_else(|| {
            api.strip_prefix("https://")
                .map(|rest| format!("wss://{rest}"))
        })
        .unwrap_or(api);
    let mut url = url::Url::parse(&format!(
        "{}/drivers/connect",
        websocket_api.trim_end_matches('/')
    ))?;
    url.query_pairs_mut()
        .append_pair("path", &driver_path)
        .append_pair("generation", &generation.to_string());
    let mut request = url.as_str().into_client_request()?;
    request.headers_mut().insert(
        "authorization",
        HeaderValue::from_str(&format!("Bearer {token}"))?,
    );
    let (mut socket, _) = connect_async(request).await?;
    send(
        &mut socket,
        &ClientMessage::Ready {
            generation,
            process_id: process::id(),
            metadata: json!({"implementation": "kas-benchmark-driver"}),
        },
    )
    .await?;

    let owner_manifest = driver_path
        .strip_suffix("/driver")
        .context("benchmark Driver path must end with /driver")?
        .to_owned();
    let mut pending = HashMap::<Uuid, String>::new();
    while let Some(message) = socket.next().await {
        match message? {
            Message::Text(text) => {
                let command: ServerMessage = serde_json::from_str(&text)?;
                match command {
                    ServerMessage::Hello { .. } => {}
                    ServerMessage::Reconcile {
                        delivery_id,
                        resource,
                    } => {
                        let owned = resource.manifest == owner_manifest;
                        if owned {
                            emit(&mut metrics, "received", &driver_path, &resource.path).await;
                        }
                        send(&mut socket, &ClientMessage::Ack { delivery_id }).await?;
                        if delay_ms > 0 {
                            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                        }
                        let operations = reconcile(&owner_manifest, &resource);
                        if owned {
                            pending.insert(delivery_id, resource.path);
                        }
                        send(
                            &mut socket,
                            &ClientMessage::Mutation {
                                request_id: delivery_id,
                                delivery_id,
                                driver_generation: generation,
                                operations,
                            },
                        )
                        .await?;
                    }
                    ServerMessage::MutationResult {
                        delivery_id,
                        status,
                        error,
                        ..
                    } => {
                        if status != MutationStatus::Committed {
                            bail!(
                                "benchmark mutation {delivery_id} rejected: {}",
                                error
                                    .map(|error| error.message)
                                    .unwrap_or_else(|| "unknown error".into())
                            );
                        }
                        if let Some(path) = pending.remove(&delivery_id) {
                            emit(&mut metrics, "completed", &driver_path, &path).await;
                        }
                    }
                    ServerMessage::Stop { generation, .. } => {
                        send(&mut socket, &ClientMessage::Stopped { generation }).await?;
                        return Ok(());
                    }
                    ServerMessage::Ping => {
                        send(&mut socket, &ClientMessage::Pong).await?;
                    }
                    ServerMessage::Run { .. } => {
                        bail!("benchmark Driver does not execute Actions");
                    }
                    ServerMessage::Error { code, message } => {
                        bail!("KAS Driver protocol error {code}: {message}");
                    }
                }
            }
            Message::Ping(payload) => socket.send(Message::Pong(payload)).await?,
            Message::Close(_) => break,
            Message::Binary(_) | Message::Pong(_) | Message::Frame(_) => {}
        }
    }
    bail!("benchmark Driver WebSocket closed unexpectedly")
}

fn reconcile(owner_manifest: &str, resource: &Resource) -> Vec<Mutation> {
    if resource.manifest != owner_manifest {
        return Vec::new();
    }
    vec![Mutation::UpdateResourceStatus {
        resource_path: resource.path.clone(),
        expected_revision: resource.revision,
        status: ResourceStatus {
            metadata: resource.status_metadata(resource.metadata.state.clone()),
            spec: resource.spec.clone(),
        },
    }]
}

async fn emit(socket: &mut TcpStream, stage: &str, driver: &str, path: &str) {
    let metric = DriverMetric {
        stage,
        driver,
        path,
        time_ns: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
            .min(u64::MAX as u128) as u64,
    };
    if let Ok(bytes) = serde_json::to_vec(&metric) {
        let _ = socket.write_all(&bytes).await;
        let _ = socket.write_all(b"\n").await;
    }
}

async fn send<S>(
    socket: &mut tokio_tungstenite::WebSocketStream<S>,
    message: &ClientMessage,
) -> anyhow::Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    socket
        .send(Message::Text(serde_json::to_string(message)?.into()))
        .await?;
    Ok(())
}
