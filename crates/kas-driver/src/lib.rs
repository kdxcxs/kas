use std::{sync::Mutex, time::Duration};

use futures_util::{SinkExt, StreamExt};
use kas_core::{
    Action, Driver as DriverRecord, DriverExecution, DriverState, Mutation, ObjectKind,
    ObjectSelector, ReconcileObject, Resource, Run, RunResult,
};
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

pub trait Driver: Send + Sync {
    fn name(&self) -> &str;

    fn reconcile(&self, object: &ReconcileObject) -> Result<Vec<Mutation>, DriverError>;

    fn execute(
        &self,
        resource: &Resource,
        action: &Action,
        run: &Run,
    ) -> Result<DriverExecution, DriverError>;

    fn watch_selectors(&self) -> Vec<WatchSelector> {
        Vec::new()
    }

    fn on_watch_event(&self, _event: &WatchEvent) -> Result<(), DriverError> {
        Ok(())
    }
}

pub type WatchSelector = ObjectSelector;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WatchObject {
    pub kind: ObjectKind,
    pub path: String,
    pub revision: Option<u64>,
    pub value: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WatchEvent {
    Created {
        watch_id: Uuid,
        cursor: u64,
        object: WatchObject,
    },
    Updated {
        watch_id: Uuid,
        cursor: u64,
        object: WatchObject,
    },
    Deleted {
        watch_id: Uuid,
        cursor: u64,
        object: WatchObject,
    },
}

impl WatchEvent {
    pub fn watch_id(&self) -> Uuid {
        match self {
            Self::Created { watch_id, .. }
            | Self::Updated { watch_id, .. }
            | Self::Deleted { watch_id, .. } => *watch_id,
        }
    }

    pub fn cursor(&self) -> u64 {
        match self {
            Self::Created { cursor, .. }
            | Self::Updated { cursor, .. }
            | Self::Deleted { cursor, .. } => *cursor,
        }
    }
}

/// A durable delivery from the KAS control plane. Reconnects may deliver the
/// same `delivery_id`; the server is responsible for idempotently applying
/// acknowledgements and mutations.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    Hello {
        delivery_id: Uuid,
        driver: DriverRecord,
        cursor: u64,
    },
    Reconcile {
        delivery_id: Uuid,
        object: ReconcileObject,
    },
    Run {
        delivery_id: Uuid,
        run: Box<Run>,
        resource: Resource,
        action: Action,
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
    WatchReady {
        request_id: Uuid,
        watch_id: Uuid,
        cursor: u64,
    },
    Created {
        watch_id: Uuid,
        cursor: u64,
        object: WatchObject,
    },
    Updated {
        watch_id: Uuid,
        cursor: u64,
        object: WatchObject,
    },
    Deleted {
        watch_id: Uuid,
        cursor: u64,
        object: WatchObject,
    },
    WatchClosed {
        watch_id: Uuid,
    },
    Error {
        request_id: Option<Uuid>,
        watch_id: Option<Uuid>,
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
    Watch {
        request_id: Uuid,
        cursor: Option<u64>,
        selectors: Vec<WatchSelector>,
    },
    Unwatch {
        watch_id: Uuid,
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
    watch_cursor: Mutex<Option<u64>>,
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
            watch_cursor: Mutex::new(None),
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
                Err(SessionError::WatchRejected { code, message }) => {
                    anyhow::bail!("Driver watch was rejected ({code}): {message}")
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
        let mut session = SessionState::default();
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
                    if self.handle(command, &mut socket, &mut session).await?
                        == SessionOutcome::Stopped
                    {
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
        session: &mut SessionState,
    ) -> Result<SessionOutcome, SessionError> {
        match message {
            ServerMessage::Hello {
                delivery_id,
                driver,
                cursor,
            } => {
                if driver.generation != self.generation {
                    return Err(SessionError::Superseded(driver.generation));
                }
                self.send(socket, ClientMessage::Ack { delivery_id })
                    .await?;
                if driver.state == DriverState::Stopping {
                    self.send(
                        socket,
                        ClientMessage::Stopped {
                            generation: self.generation,
                        },
                    )
                    .await?;
                    return Ok(SessionOutcome::Stopped);
                }
                let selectors = self.implementation.watch_selectors();
                if !selectors.is_empty() {
                    let watch_cursor = {
                        let mut saved = self.watch_cursor.lock().map_err(|_| {
                            SessionError::Other(anyhow::anyhow!("watch cursor lock is poisoned"))
                        })?;
                        *saved.get_or_insert(cursor)
                    };
                    let request_id = Uuid::new_v4();
                    self.send(
                        socket,
                        ClientMessage::Watch {
                            request_id,
                            cursor: Some(watch_cursor),
                            selectors,
                        },
                    )
                    .await?;
                    session.watch_request_id = Some(request_id);
                }
            }
            ServerMessage::Reconcile {
                delivery_id,
                object,
            } => {
                self.send(socket, ClientMessage::Ack { delivery_id })
                    .await?;
                let operations = match self.implementation.reconcile(&object) {
                    Ok(operations) => operations,
                    Err(error) => {
                        eprintln!("Driver reconciliation failed and will be retried: {error}");
                        Vec::new()
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
                    match self.implementation.execute(&resource, &action, &run) {
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
                    run_path: run.path,
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
            ServerMessage::WatchReady {
                request_id,
                watch_id,
                cursor,
            } => {
                if session.watch_request_id != Some(request_id) {
                    return Err(SessionError::Other(anyhow::anyhow!(
                        "watch_ready references unknown request_id {request_id}"
                    )));
                }
                session.watch_id = Some(watch_id);
                *self.watch_cursor.lock().map_err(|_| {
                    SessionError::Other(anyhow::anyhow!("watch cursor lock is poisoned"))
                })? = Some(cursor);
            }
            ServerMessage::Created {
                watch_id,
                cursor,
                object,
            } => {
                self.handle_watch_event(
                    session,
                    WatchEvent::Created {
                        watch_id,
                        cursor,
                        object,
                    },
                )?;
            }
            ServerMessage::Updated {
                watch_id,
                cursor,
                object,
            } => {
                self.handle_watch_event(
                    session,
                    WatchEvent::Updated {
                        watch_id,
                        cursor,
                        object,
                    },
                )?;
            }
            ServerMessage::Deleted {
                watch_id,
                cursor,
                object,
            } => {
                self.handle_watch_event(
                    session,
                    WatchEvent::Deleted {
                        watch_id,
                        cursor,
                        object,
                    },
                )?;
            }
            ServerMessage::WatchClosed { watch_id } => {
                if session.watch_id == Some(watch_id) {
                    return Err(SessionError::WatchRejected {
                        code: "watch_closed".to_owned(),
                        message: format!("watch {watch_id} was closed by the control plane"),
                    });
                }
            }
            ServerMessage::Error {
                request_id,
                watch_id,
                code,
                message,
            } => {
                let is_watch_error = request_id
                    .is_some_and(|id| session.watch_request_id == Some(id))
                    || watch_id.is_some_and(|id| session.watch_id == Some(id));
                if is_watch_error {
                    return Err(SessionError::WatchRejected { code, message });
                }
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

    fn handle_watch_event(
        &self,
        session: &SessionState,
        event: WatchEvent,
    ) -> Result<(), SessionError> {
        if session.watch_id != Some(event.watch_id()) {
            return Err(SessionError::Other(anyhow::anyhow!(
                "watch event references unknown watch_id {}",
                event.watch_id()
            )));
        }
        self.implementation
            .on_watch_event(&event)
            .map_err(|error| SessionError::WatchRejected {
                code: "watch_callback_failed".to_owned(),
                message: error.to_string(),
            })?;
        *self
            .watch_cursor
            .lock()
            .map_err(|_| SessionError::Other(anyhow::anyhow!("watch cursor lock is poisoned")))? =
            Some(event.cursor());
        Ok(())
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

#[derive(Default)]
struct SessionState {
    watch_request_id: Option<Uuid>,
    watch_id: Option<Uuid>,
}

#[derive(Debug, Error)]
enum SessionError {
    #[error("Driver generation was superseded by {0}")]
    Superseded(u64),
    #[error("Driver mutation was rejected ({code}): {message}")]
    MutationRejected { code: String, message: String },
    #[error("Driver watch was rejected ({code}): {message}")]
    WatchRejected { code: String, message: String },
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    struct Noop;

    impl Driver for Noop {
        fn name(&self) -> &str {
            "noop"
        }

        fn reconcile(&self, _: &ReconcileObject) -> Result<Vec<Mutation>, DriverError> {
            Ok(Vec::new())
        }

        fn execute(
            &self,
            _: &Resource,
            _: &Action,
            _: &Run,
        ) -> Result<DriverExecution, DriverError> {
            Ok(Value::Null.into())
        }
    }

    struct Watching {
        events: Arc<Mutex<Vec<WatchEvent>>>,
    }

    impl Driver for Watching {
        fn name(&self) -> &str {
            "watching"
        }

        fn reconcile(&self, _: &ReconcileObject) -> Result<Vec<Mutation>, DriverError> {
            Ok(Vec::new())
        }

        fn execute(
            &self,
            _: &Resource,
            _: &Action,
            _: &Run,
        ) -> Result<DriverExecution, DriverError> {
            Ok(Value::Null.into())
        }

        fn watch_selectors(&self) -> Vec<WatchSelector> {
            vec![ObjectSelector {
                kind: Some(kas_core::KindSelector::One(ObjectKind::Resource)),
                ..ObjectSelector::default()
            }]
        }

        fn on_watch_event(&self, event: &WatchEvent) -> Result<(), DriverError> {
            self.events.lock().unwrap().push(event.clone());
            Ok(())
        }
    }

    fn driver_record(driver_path: &str, generation: u64) -> DriverRecord {
        serde_json::from_value(json!({
            "path": driver_path,
            "desired_state": "running",
            "state": "ready",
            "generation": generation,
            "process_id": null,
            "metadata": null,
            "started_at": "2026-01-01T00:00:00Z",
            "heartbeat_at": "2026-01-01T00:00:00Z",
            "stopped_at": null,
            "error": null,
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z"
        }))
        .unwrap()
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
    fn watch_messages_have_stable_tagged_wire_format() {
        let request_id = Uuid::parse_str("10000000-0000-0000-0000-000000000001").unwrap();
        let watch_id = Uuid::parse_str("20000000-0000-0000-0000-000000000002").unwrap();
        let watch = ClientMessage::Watch {
            request_id,
            cursor: Some(12),
            selectors: vec![ObjectSelector {
                kind: Some(kas_core::KindSelector::One(ObjectKind::Resource)),
                path: Some("/examples/**".into()),
                ..ObjectSelector::default()
            }],
        };
        assert_eq!(
            serde_json::to_value(watch).unwrap(),
            json!({
                "type": "watch",
                "request_id": request_id,
                "cursor": 12,
                "selectors": [{
                    "kind": "resource",
                    "path": "/examples/**"
                }]
            })
        );

        let created = ServerMessage::Created {
            watch_id,
            cursor: 13,
            object: WatchObject {
                kind: ObjectKind::Resource,
                path: "/resources/example".into(),
                revision: Some(2),
                value: json!({"name": "example"}),
            },
        };
        assert_eq!(
            serde_json::to_value(created).unwrap(),
            json!({
                "type": "created",
                "watch_id": watch_id,
                "cursor": 13,
                "object": {
                    "kind": "resource",
                    "path": "/resources/example",
                    "revision": 2,
                    "value": {"name": "example"}
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

    #[tokio::test]
    async fn watch_resumes_from_last_processed_cursor_after_reconnect() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let driver_path = "/drivers/example";
        let first_delivery = Uuid::new_v4();
        let second_delivery = Uuid::new_v4();
        let first_watch = Uuid::new_v4();
        let second_watch = Uuid::new_v4();
        let object_path = "/resources/example";
        let record = driver_record(driver_path, 9);

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            assert!(matches!(
                serde_json::from_str::<ClientMessage>(
                    &socket.next().await.unwrap().unwrap().into_text().unwrap()
                )
                .unwrap(),
                ClientMessage::Ready { generation: 9, .. }
            ));
            socket
                .send(Message::Text(
                    serde_json::to_string(&ServerMessage::Hello {
                        delivery_id: first_delivery,
                        driver: record.clone(),
                        cursor: 40,
                    })
                    .unwrap()
                    .into(),
                ))
                .await
                .unwrap();
            assert!(matches!(
                serde_json::from_str::<ClientMessage>(
                    &socket.next().await.unwrap().unwrap().into_text().unwrap()
                )
                .unwrap(),
                ClientMessage::Ack { delivery_id } if delivery_id == first_delivery
            ));
            let first_request = match serde_json::from_str::<ClientMessage>(
                &socket.next().await.unwrap().unwrap().into_text().unwrap(),
            )
            .unwrap()
            {
                ClientMessage::Watch {
                    request_id,
                    cursor: Some(40),
                    ..
                } => request_id,
                message => panic!("unexpected message: {message:?}"),
            };
            socket
                .send(Message::Text(
                    serde_json::to_string(&ServerMessage::WatchReady {
                        request_id: first_request,
                        watch_id: first_watch,
                        cursor: 40,
                    })
                    .unwrap()
                    .into(),
                ))
                .await
                .unwrap();
            socket
                .send(Message::Text(
                    serde_json::to_string(&ServerMessage::Created {
                        watch_id: first_watch,
                        cursor: 41,
                        object: WatchObject {
                            kind: ObjectKind::Resource,
                            path: object_path.into(),
                            revision: Some(0),
                            value: json!({"pass": 1}),
                        },
                    })
                    .unwrap()
                    .into(),
                ))
                .await
                .unwrap();
            socket.close(None).await.unwrap();

            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let _ready = socket.next().await.unwrap().unwrap();
            socket
                .send(Message::Text(
                    serde_json::to_string(&ServerMessage::Hello {
                        delivery_id: second_delivery,
                        driver: record,
                        cursor: 99,
                    })
                    .unwrap()
                    .into(),
                ))
                .await
                .unwrap();
            let _ack = socket.next().await.unwrap().unwrap();
            let second_request = match serde_json::from_str::<ClientMessage>(
                &socket.next().await.unwrap().unwrap().into_text().unwrap(),
            )
            .unwrap()
            {
                ClientMessage::Watch {
                    request_id,
                    cursor: Some(41),
                    ..
                } => request_id,
                message => panic!("unexpected message: {message:?}"),
            };
            socket
                .send(Message::Text(
                    serde_json::to_string(&ServerMessage::WatchReady {
                        request_id: second_request,
                        watch_id: second_watch,
                        cursor: 41,
                    })
                    .unwrap()
                    .into(),
                ))
                .await
                .unwrap();
            socket
                .send(Message::Text(
                    serde_json::to_string(&ServerMessage::Updated {
                        watch_id: second_watch,
                        cursor: 42,
                        object: WatchObject {
                            kind: ObjectKind::Resource,
                            path: object_path.into(),
                            revision: Some(1),
                            value: json!({"pass": 2}),
                        },
                    })
                    .unwrap()
                    .into(),
                ))
                .await
                .unwrap();
            socket
                .send(Message::Text(
                    serde_json::to_string(&ServerMessage::Stop {
                        delivery_id: second_delivery,
                        generation: 9,
                    })
                    .unwrap()
                    .into(),
                ))
                .await
                .unwrap();
            let _ack = socket.next().await.unwrap().unwrap();
            let _stopped = socket.next().await.unwrap().unwrap();
        });

        let events = Arc::new(Mutex::new(Vec::new()));
        let runtime = DriverRuntime::new(
            format!("http://{address}"),
            driver_path,
            9,
            "secret",
            Watching {
                events: events.clone(),
            },
        )
        .with_reconnect_interval(Duration::from_millis(10));
        runtime.run().await.unwrap();
        server.await.unwrap();
        assert_eq!(
            events
                .lock()
                .unwrap()
                .iter()
                .map(WatchEvent::cursor)
                .collect::<Vec<_>>(),
            vec![41, 42]
        );
    }
}
