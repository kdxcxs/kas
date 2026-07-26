use std::time::Duration;

use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use kas_core::{DriverExecution, DriverState, Mutation, Resource, RunResult};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use thiserror::Error;
use tokio::net::TcpStream;
use tokio_tungstenite::{
    connect_async,
    tungstenite::{client::IntoClientRequest, http::HeaderValue, Message},
    MaybeTlsStream, WebSocketStream,
};
use uuid::Uuid;

#[derive(Debug, Error)]
pub enum DriverError {
    #[error("unsupported action: {0}")]
    UnsupportedAction(String),
    #[error("driver execution failed: {0}")]
    Execution(String),
}

#[async_trait]
pub trait Driver: Send + Sync {
    fn name(&self) -> &str;

    async fn reconcile(&self, resource: &Resource) -> Result<Vec<Mutation>, DriverError>;

    async fn execute(
        &self,
        resource: &Resource,
        action: &Resource,
        run: &Resource,
    ) -> Result<DriverExecution, DriverError>;
}

/// A durable delivery from the KAS control plane. Reconnects may deliver the
/// same `delivery_id`; the server is responsible for idempotently applying
/// acknowledgements and mutations.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    Hello {
        delivery_id: Uuid,
        driver: Resource,
    },
    Reconcile {
        delivery_id: Uuid,
        resource: Resource,
    },
    Run {
        delivery_id: Uuid,
        run: Resource,
        resource: Resource,
        action: Resource,
    },
    Stop {
        delivery_id: Uuid,
        generation: u64,
    },
    MutationResult {
        request_id: Uuid,
        delivery_id: Uuid,
        status: MutationStatus,
        #[serde(default)]
        results: Vec<Value>,
        #[serde(default)]
        error: Option<MutationError>,
    },
    Error {
        code: String,
        message: String,
    },
    Ping,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MutationStatus {
    Committed,
    Rejected,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MutationError {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    Ready {
        generation: u64,
        process_id: u32,
        metadata: Value,
    },
    Ack {
        delivery_id: Uuid,
    },
    Mutation {
        request_id: Uuid,
        delivery_id: Uuid,
        driver_generation: u64,
        operations: Vec<Mutation>,
    },
    Stopped {
        generation: u64,
    },
    Pong,
}

pub struct DriverRuntime<D> {
    api: String,
    driver_path: String,
    generation: u64,
    token: String,
    implementation: D,
    reconnect_interval: Duration,
}

impl<D: Driver> DriverRuntime<D> {
    pub fn new(
        api: impl Into<String>,
        driver_path: impl Into<String>,
        generation: u64,
        token: impl Into<String>,
        implementation: D,
    ) -> Self {
        Self {
            api: api.into().trim_end_matches('/').to_owned(),
            driver_path: driver_path.into(),
            generation,
            token: token.into(),
            implementation,
            reconnect_interval: Duration::from_millis(250),
        }
    }

    pub fn with_reconnect_interval(mut self, interval: Duration) -> Self {
        self.reconnect_interval = interval;
        self
    }

    pub async fn run(&self) -> anyhow::Result<()> {
        loop {
            match self.connect_and_serve().await {
                Ok(SessionOutcome::Stopped) => return Ok(()),
                Ok(SessionOutcome::Reconnect) => {}
                Err(SessionError::Superseded(actual)) => anyhow::bail!(
                    "Driver generation {} was superseded by {actual}",
                    self.generation
                ),
                Err(SessionError::MutationRejected { code, message }) => {
                    anyhow::bail!("Driver mutation was rejected ({code}): {message}")
                }
                Err(SessionError::Other(error)) => {
                    eprintln!("driver WebSocket disconnected: {error:#}");
                }
            }
            tokio::time::sleep(self.reconnect_interval).await;
        }
    }

    async fn connect_and_serve(&self) -> Result<SessionOutcome, SessionError> {
        let (mut socket, _) = connect_async(self.connection_request()?)
            .await
            .map_err(|error| SessionError::Other(error.into()))?;
        self.send(
            &mut socket,
            ClientMessage::Ready {
                generation: self.generation,
                process_id: std::process::id(),
                metadata: json!({ "implementation": self.implementation.name() }),
            },
        )
        .await?;

        while let Some(message) = socket.next().await {
            let message = message.map_err(|error| SessionError::Other(error.into()))?;
            match message {
                Message::Text(text) => {
                    let command: ServerMessage = serde_json::from_str(&text)
                        .map_err(|error| SessionError::Other(error.into()))?;
                    if self.handle(command, &mut socket).await? == SessionOutcome::Stopped {
                        return Ok(SessionOutcome::Stopped);
                    }
                }
                Message::Ping(payload) => socket
                    .send(Message::Pong(payload))
                    .await
                    .map_err(|error| SessionError::Other(error.into()))?,
                Message::Close(_) => return Ok(SessionOutcome::Reconnect),
                Message::Binary(_) | Message::Pong(_) | Message::Frame(_) => {}
            }
        }
        Ok(SessionOutcome::Reconnect)
    }

    fn connection_request(
        &self,
    ) -> Result<tokio_tungstenite::tungstenite::http::Request<()>, SessionError> {
        let websocket_api = self
            .api
            .strip_prefix("https://")
            .map(|rest| format!("wss://{rest}"))
            .or_else(|| {
                self.api
                    .strip_prefix("http://")
                    .map(|rest| format!("ws://{rest}"))
            })
            .unwrap_or_else(|| self.api.clone());
        let mut url = url::Url::parse(&format!("{websocket_api}/drivers/connect"))
            .map_err(|error| SessionError::Other(error.into()))?;
        url.query_pairs_mut()
            .append_pair("path", &self.driver_path)
            .append_pair("generation", &self.generation.to_string());
        let mut request = url
            .as_str()
            .into_client_request()
            .map_err(|error| SessionError::Other(error.into()))?;
        request.headers_mut().insert(
            "authorization",
            HeaderValue::from_str(&format!("Bearer {}", self.token))
                .map_err(|error| SessionError::Other(error.into()))?,
        );
        Ok(request)
    }

    async fn handle(
        &self,
        message: ServerMessage,
        socket: &mut WebSocketStream<MaybeTlsStream<TcpStream>>,
    ) -> Result<SessionOutcome, SessionError> {
        match message {
            ServerMessage::Hello {
                delivery_id,
                driver,
            } => {
                self.send(socket, ClientMessage::Ack { delivery_id })
                    .await?;
                let state: DriverState =
                    serde_json::from_value(serde_json::Value::String(driver.status.metadata.state))
                        .map_err(|error| SessionError::Other(error.into()))?;
                if state == DriverState::Stopping {
                    self.send(
                        socket,
                        ClientMessage::Stopped {
                            generation: self.generation,
                        },
                    )
                    .await?;
                    return Ok(SessionOutcome::Stopped);
                }
            }
            ServerMessage::Reconcile {
                delivery_id,
                resource,
            } => {
                self.send(socket, ClientMessage::Ack { delivery_id })
                    .await?;
                let operations = match self.implementation.reconcile(&resource).await {
                    Ok(operations) => operations,
                    Err(error) => {
                        eprintln!("Driver reconciliation failed and will be retried: {error}");
                        return Err(SessionError::Other(anyhow::anyhow!(error.to_string())));
                    }
                };
                self.send(
                    socket,
                    ClientMessage::Mutation {
                        request_id: delivery_id,
                        delivery_id,
                        driver_generation: self.generation,
                        operations,
                    },
                )
                .await?;
            }
            ServerMessage::Run {
                delivery_id,
                run,
                resource,
                action,
            } => {
                self.send(socket, ClientMessage::Ack { delivery_id })
                    .await?;
                let (result, mut mutations) =
                    match self.implementation.execute(&resource, &action, &run).await {
                        Ok(execution) => (
                            RunResult::Succeeded {
                                output: execution.output,
                            },
                            execution.mutations,
                        ),
                        Err(error) => (
                            RunResult::Failed {
                                error: error.to_string(),
                            },
                            Vec::new(),
                        ),
                    };
                mutations.push(Mutation::CompleteRun {
                    run_path: run.path.clone(),
                    result,
                });
                self.send(
                    socket,
                    ClientMessage::Mutation {
                        request_id: delivery_id,
                        delivery_id,
                        driver_generation: self.generation,
                        operations: mutations,
                    },
                )
                .await?;
            }
            ServerMessage::MutationResult {
                request_id,
                delivery_id,
                status,
                results: _,
                error,
            } => {
                if request_id != delivery_id {
                    return Err(SessionError::Other(anyhow::anyhow!(
                        "mutation result request_id {request_id} does not match delivery_id {delivery_id}"
                    )));
                }
                if status == MutationStatus::Rejected {
                    let error = error.unwrap_or(MutationError {
                        code: "mutation_rejected".to_owned(),
                        message: "the control plane rejected the mutation".to_owned(),
                    });
                    return Err(SessionError::MutationRejected {
                        code: error.code,
                        message: error.message,
                    });
                }
            }
            ServerMessage::Error { code, message } => {
                return Err(SessionError::Other(anyhow::anyhow!(
                    "control plane error ({code}): {message}"
                )));
            }
            ServerMessage::Stop {
                delivery_id,
                generation,
            } => {
                if generation != self.generation {
                    return Err(SessionError::Superseded(generation));
                }
                self.send(socket, ClientMessage::Ack { delivery_id })
                    .await?;
                self.send(
                    socket,
                    ClientMessage::Stopped {
                        generation: self.generation,
                    },
                )
                .await?;
                return Ok(SessionOutcome::Stopped);
            }
            ServerMessage::Ping => self.send(socket, ClientMessage::Pong).await?,
        }
        Ok(SessionOutcome::Reconnect)
    }

    async fn send(
        &self,
        socket: &mut WebSocketStream<MaybeTlsStream<TcpStream>>,
        message: ClientMessage,
    ) -> Result<(), SessionError> {
        let json =
            serde_json::to_string(&message).map_err(|error| SessionError::Other(error.into()))?;
        socket
            .send(Message::Text(json.into()))
            .await
            .map_err(|error| SessionError::Other(error.into()))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionOutcome {
    Reconnect,
    Stopped,
}

#[derive(Debug, Error)]
enum SessionError {
    #[error("Driver generation was superseded by {0}")]
    Superseded(u64),
    #[error("Driver mutation was rejected ({code}): {message}")]
    MutationRejected { code: String, message: String },
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Noop;

    #[async_trait]
    impl Driver for Noop {
        fn name(&self) -> &str {
            "noop"
        }

        async fn reconcile(&self, _: &Resource) -> Result<Vec<Mutation>, DriverError> {
            Ok(Vec::new())
        }

        async fn execute(
            &self,
            _: &Resource,
            _: &Resource,
            _: &Resource,
        ) -> Result<DriverExecution, DriverError> {
            Ok(Value::Null.into())
        }
    }

    #[test]
    fn mutation_has_stable_tagged_wire_format() {
        let delivery_id = Uuid::parse_str("10000000-0000-0000-0000-000000000001").unwrap();
        let run_path = "/runs/example".to_owned();
        let mutation = ClientMessage::Mutation {
            request_id: delivery_id,
            delivery_id,
            driver_generation: 7,
            operations: vec![Mutation::CompleteRun {
                run_path: run_path.clone(),
                result: RunResult::Succeeded {
                    output: json!({"message": "done"}),
                },
            }],
        };

        assert_eq!(
            serde_json::to_value(mutation).unwrap(),
            json!({
                "type": "mutation",
                "request_id": delivery_id,
                "delivery_id": delivery_id,
                "driver_generation": 7,
                "operations": [{
                    "operation": "complete_run",
                    "run_path": run_path,
                    "result": {
                        "status": "succeeded",
                        "output": {"message": "done"}
                    }
                }]
            })
        );
    }

    #[test]
    fn mutation_result_has_stable_tagged_wire_format() {
        let delivery_id = Uuid::parse_str("10000000-0000-0000-0000-000000000001").unwrap();
        let result = ServerMessage::MutationResult {
            request_id: delivery_id,
            delivery_id,
            status: MutationStatus::Rejected,
            results: vec![],
            error: Some(MutationError {
                code: "revision_conflict".to_owned(),
                message: "resource revision is stale".to_owned(),
            }),
        };

        assert_eq!(
            serde_json::to_value(result).unwrap(),
            json!({
                "type": "mutation_result",
                "request_id": delivery_id,
                "delivery_id": delivery_id,
                "status": "rejected",
                "results": [],
                "error": {
                    "code": "revision_conflict",
                    "message": "resource revision is stale"
                }
            })
        );
    }

    #[test]
    fn connection_request_uses_websocket_and_bearer_auth() {
        let driver_path = "/drivers/example";
        let runtime =
            DriverRuntime::new("https://kas.example.test/", driver_path, 9, "secret", Noop);
        let request = runtime.connection_request().unwrap();

        assert_eq!(
            request.uri().to_string(),
            "wss://kas.example.test/drivers/connect?path=%2Fdrivers%2Fexample&generation=9"
        );
        assert_eq!(request.headers()["authorization"], "Bearer secret");
    }

    #[tokio::test]
    async fn committed_mutation_allows_runtime_to_continue_until_stop() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let delivery_id = Uuid::parse_str("40000000-0000-0000-0000-000000000004").unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();

            let ready = socket.next().await.unwrap().unwrap().into_text().unwrap();
            assert!(matches!(
                serde_json::from_str::<ClientMessage>(&ready).unwrap(),
                ClientMessage::Ready { generation: 9, .. }
            ));

            socket
                .send(Message::Text(
                    serde_json::to_string(&ServerMessage::MutationResult {
                        request_id: delivery_id,
                        delivery_id,
                        status: MutationStatus::Committed,
                        results: vec![json!({"revision": 2})],
                        error: None,
                    })
                    .unwrap()
                    .into(),
                ))
                .await
                .unwrap();
            socket
                .send(Message::Text(
                    serde_json::to_string(&ServerMessage::Stop {
                        delivery_id,
                        generation: 9,
                    })
                    .unwrap()
                    .into(),
                ))
                .await
                .unwrap();

            let ack = socket.next().await.unwrap().unwrap().into_text().unwrap();
            assert!(matches!(
                serde_json::from_str::<ClientMessage>(&ack).unwrap(),
                ClientMessage::Ack { delivery_id: id } if id == delivery_id
            ));
            let stopped = socket.next().await.unwrap().unwrap().into_text().unwrap();
            assert!(matches!(
                serde_json::from_str::<ClientMessage>(&stopped).unwrap(),
                ClientMessage::Stopped { generation: 9 }
            ));
        });

        let runtime = DriverRuntime::new(
            format!("http://{address}"),
            "/drivers/example",
            9,
            "secret",
            Noop,
        );
        assert_eq!(
            runtime.connect_and_serve().await.unwrap(),
            SessionOutcome::Stopped
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn rejected_mutation_terminates_runtime_without_reconnecting() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let delivery_id = Uuid::parse_str("50000000-0000-0000-0000-000000000005").unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let ready = socket.next().await.unwrap().unwrap().into_text().unwrap();
            assert!(matches!(
                serde_json::from_str::<ClientMessage>(&ready).unwrap(),
                ClientMessage::Ready { generation: 9, .. }
            ));
            socket
                .send(Message::Text(
                    serde_json::to_string(&ServerMessage::MutationResult {
                        request_id: delivery_id,
                        delivery_id,
                        status: MutationStatus::Rejected,
                        results: vec![],
                        error: Some(MutationError {
                            code: "permission_denied".to_owned(),
                            message: "not allowed".to_owned(),
                        }),
                    })
                    .unwrap()
                    .into(),
                ))
                .await
                .unwrap();

            assert!(
                tokio::time::timeout(Duration::from_millis(500), listener.accept())
                    .await
                    .is_err(),
                "the runtime unexpectedly reconnected after a rejected mutation"
            );
        });

        let runtime = DriverRuntime::new(
            format!("http://{address}"),
            "/drivers/example",
            9,
            "secret",
            Noop,
        )
        .with_reconnect_interval(Duration::from_millis(10));
        let error = runtime.run().await.unwrap_err();
        assert!(error
            .to_string()
            .contains("Driver mutation was rejected (permission_denied): not allowed"));
        server.await.unwrap();
    }
}
