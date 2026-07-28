use std::{collections::HashMap, sync::Arc, time::Duration};

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
    ReconcileCompleteResult {
        delivery_id: Uuid,
        status: CompletionStatus,
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

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CompletionStatus {
    Completed,
    AlreadyCompleted,
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
    ReconcileComplete {
        delivery_id: Uuid,
        driver_generation: u64,
    },
    Heartbeat {
        delivery_ids: Vec<Uuid>,
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
    implementation: Arc<D>,
    reconnect_interval: Duration,
}

impl<D: Driver + 'static> DriverRuntime<D> {
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
            implementation: Arc::new(implementation),
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
        let (outbound_tx, mut outbound_rx) = tokio::sync::mpsc::unbounded_channel();
        let mutation_waiters = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
        let completion_waiters = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
        let mut active = HashMap::<Uuid, tokio::task::JoinHandle<()>>::new();
        let mut heartbeat = tokio::time::interval(Duration::from_secs(5));
        heartbeat.tick().await;

        let outcome = loop {
            active.retain(|_, task| !task.is_finished());
            tokio::select! {
                Some(message) = outbound_rx.recv() => {
                    self.send(&mut socket, message).await?;
                }
                _ = heartbeat.tick() => {
                    active.retain(|_, task| !task.is_finished());
                    if !active.is_empty() {
                        self.send(
                            &mut socket,
                            ClientMessage::Heartbeat {
                                delivery_ids: active.keys().copied().collect(),
                            },
                        ).await?;
                    }
                }
                incoming = socket.next() => {
                    let Some(message) = incoming else {
                        break SessionOutcome::Reconnect;
                    };
                    let message = message.map_err(|error| SessionError::Other(error.into()))?;
                    let Message::Text(text) = message else {
                        match message {
                            Message::Ping(payload) => socket
                                .send(Message::Pong(payload))
                                .await
                                .map_err(|error| SessionError::Other(error.into()))?,
                            Message::Close(_) => break SessionOutcome::Reconnect,
                            Message::Binary(_) | Message::Pong(_) | Message::Frame(_) => {}
                            Message::Text(_) => unreachable!(),
                        }
                        continue;
                    };
                    let command: ServerMessage = serde_json::from_str(&text)
                        .map_err(|error| SessionError::Other(error.into()))?;
                    match command {
                        ServerMessage::Hello { delivery_id, driver } => {
                            self.send(&mut socket, ClientMessage::Ack { delivery_id }).await?;
                            let state: DriverState = serde_json::from_value(
                                Value::String(driver.status.metadata.state),
                            )
                            .map_err(|error| SessionError::Other(error.into()))?;
                            if state == DriverState::Stopping {
                                self.send(
                                    &mut socket,
                                    ClientMessage::Stopped {
                                        generation: self.generation,
                                    },
                                )
                                .await?;
                                break SessionOutcome::Stopped;
                            }
                        }
                        ServerMessage::Reconcile { delivery_id, resource } => {
                            self.send(&mut socket, ClientMessage::Ack { delivery_id }).await?;
                            active.retain(|_, task| !task.is_finished());
                            if let std::collections::hash_map::Entry::Vacant(entry) =
                                active.entry(delivery_id)
                            {
                                entry.insert(spawn_reconciliation(
                                    self.implementation.clone(),
                                    self.generation,
                                    delivery_id,
                                    resource,
                                    outbound_tx.clone(),
                                    mutation_waiters.clone(),
                                    completion_waiters.clone(),
                                ));
                            }
                        }
                        ServerMessage::Run {
                            delivery_id,
                            run,
                            resource,
                            action,
                        } => {
                            self.send(&mut socket, ClientMessage::Ack { delivery_id }).await?;
                            active.retain(|_, task| !task.is_finished());
                            if let std::collections::hash_map::Entry::Vacant(entry) =
                                active.entry(delivery_id)
                            {
                                entry.insert(spawn_run(
                                    self.implementation.clone(),
                                    self.generation,
                                    delivery_id,
                                    run,
                                    resource,
                                    action,
                                    outbound_tx.clone(),
                                    mutation_waiters.clone(),
                                ));
                            }
                        }
                        ServerMessage::MutationResult {
                            request_id,
                            status,
                            error,
                            ..
                        } => {
                            if let Some(waiter) = mutation_waiters.lock().await.remove(&request_id) {
                                let result = if status == MutationStatus::Committed {
                                    Ok(())
                                } else {
                                    Err(error.unwrap_or(MutationError {
                                        code: "mutation_rejected".to_owned(),
                                        message: "the control plane rejected the mutation".to_owned(),
                                    }))
                                };
                                let _ = waiter.send(result);
                            }
                        }
                        ServerMessage::ReconcileCompleteResult {
                            delivery_id,
                            status,
                        } => {
                            if let Some(waiter) =
                                completion_waiters.lock().await.remove(&delivery_id)
                            {
                                let _ = waiter.send(status);
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
                            self.send(&mut socket, ClientMessage::Ack { delivery_id }).await?;
                            self.send(
                                &mut socket,
                                ClientMessage::Stopped {
                                    generation: self.generation,
                                },
                            )
                            .await?;
                            break SessionOutcome::Stopped;
                        }
                        ServerMessage::Ping => {
                            self.send(&mut socket, ClientMessage::Pong).await?;
                        }
                    }
                }
            }
        };
        for (_, task) in active {
            task.abort();
        }
        Ok(outcome)
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

type MutationWaiters =
    Arc<tokio::sync::Mutex<HashMap<Uuid, tokio::sync::oneshot::Sender<Result<(), MutationError>>>>>;
type CompletionWaiters =
    Arc<tokio::sync::Mutex<HashMap<Uuid, tokio::sync::oneshot::Sender<CompletionStatus>>>>;

fn spawn_reconciliation<D: Driver + 'static>(
    implementation: Arc<D>,
    generation: u64,
    delivery_id: Uuid,
    resource: Resource,
    outbound: tokio::sync::mpsc::UnboundedSender<ClientMessage>,
    mutation_waiters: MutationWaiters,
    completion_waiters: CompletionWaiters,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let operations = match implementation.reconcile(&resource).await {
            Ok(operations) => operations,
            Err(error) => {
                eprintln!(
                    "Driver reconciliation {delivery_id} failed and will be retried: {error}"
                );
                return;
            }
        };
        if !operations.is_empty() {
            // The current Driver trait emits one mutation batch per
            // reconciliation. Deriving its request ID from the stable
            // delivery ID makes a replay after reconnect idempotent.
            let request_id = delivery_id;
            let (result_tx, result_rx) = tokio::sync::oneshot::channel();
            mutation_waiters.lock().await.insert(request_id, result_tx);
            if outbound
                .send(ClientMessage::Mutation {
                    request_id,
                    delivery_id,
                    driver_generation: generation,
                    operations,
                })
                .is_err()
            {
                mutation_waiters.lock().await.remove(&request_id);
                return;
            }
            match result_rx.await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    eprintln!(
                        "Driver reconciliation {delivery_id} mutation was rejected ({}): {}",
                        error.code, error.message
                    );
                    return;
                }
                Err(_) => return,
            }
        }

        let (complete_tx, complete_rx) = tokio::sync::oneshot::channel();
        completion_waiters
            .lock()
            .await
            .insert(delivery_id, complete_tx);
        if outbound
            .send(ClientMessage::ReconcileComplete {
                delivery_id,
                driver_generation: generation,
            })
            .is_err()
        {
            completion_waiters.lock().await.remove(&delivery_id);
            return;
        }
        let _ = complete_rx.await;
    })
}

#[allow(clippy::too_many_arguments)]
fn spawn_run<D: Driver + 'static>(
    implementation: Arc<D>,
    generation: u64,
    delivery_id: Uuid,
    run: Resource,
    resource: Resource,
    action: Resource,
    outbound: tokio::sync::mpsc::UnboundedSender<ClientMessage>,
    mutation_waiters: MutationWaiters,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let (result, mut mutations) = match implementation.execute(&resource, &action, &run).await {
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
        let request_id = delivery_id;
        let (result_tx, result_rx) = tokio::sync::oneshot::channel();
        mutation_waiters.lock().await.insert(request_id, result_tx);
        if outbound
            .send(ClientMessage::Mutation {
                request_id,
                delivery_id,
                driver_generation: generation,
                operations: mutations,
            })
            .is_err()
        {
            mutation_waiters.lock().await.remove(&request_id);
            return;
        }
        if let Ok(Err(error)) = result_rx.await {
            eprintln!(
                "Driver run {delivery_id} mutation was rejected ({}): {}",
                error.code, error.message
            );
        }
    })
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
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

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

    struct SlowNoop(Arc<AtomicUsize>);

    #[async_trait]
    impl Driver for SlowNoop {
        fn name(&self) -> &str {
            "slow-noop"
        }

        async fn reconcile(&self, _: &Resource) -> Result<Vec<Mutation>, DriverError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(100)).await;
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

    #[tokio::test]
    async fn duplicate_reconcile_is_deduplicated_and_explicitly_completed() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let delivery_id = Uuid::parse_str("50000000-0000-0000-0000-000000000005").unwrap();
        let resource = Resource {
            path: "/resources/example".into(),
            metadata: Default::default(),
            spec: json!({}),
            status: Default::default(),
        };
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let ready = socket.next().await.unwrap().unwrap().into_text().unwrap();
            assert!(matches!(
                serde_json::from_str::<ClientMessage>(&ready).unwrap(),
                ClientMessage::Ready { generation: 9, .. }
            ));
            let message = ServerMessage::Reconcile {
                delivery_id,
                resource,
            };
            for _ in 0..2 {
                socket
                    .send(Message::Text(
                        serde_json::to_string(&message).unwrap().into(),
                    ))
                    .await
                    .unwrap();
            }
            for _ in 0..2 {
                let ack = socket.next().await.unwrap().unwrap().into_text().unwrap();
                assert!(matches!(
                    serde_json::from_str::<ClientMessage>(&ack).unwrap(),
                    ClientMessage::Ack { delivery_id: id } if id == delivery_id
                ));
            }
            let complete = socket.next().await.unwrap().unwrap().into_text().unwrap();
            assert!(matches!(
                serde_json::from_str::<ClientMessage>(&complete).unwrap(),
                ClientMessage::ReconcileComplete {
                    delivery_id: id,
                    driver_generation: 9,
                } if id == delivery_id
            ));
            socket
                .send(Message::Text(
                    serde_json::to_string(&ServerMessage::ReconcileCompleteResult {
                        delivery_id,
                        status: CompletionStatus::Completed,
                    })
                    .unwrap()
                    .into(),
                ))
                .await
                .unwrap();
            let stop_id = Uuid::new_v4();
            socket
                .send(Message::Text(
                    serde_json::to_string(&ServerMessage::Stop {
                        delivery_id: stop_id,
                        generation: 9,
                    })
                    .unwrap()
                    .into(),
                ))
                .await
                .unwrap();
            for expected in ["ack", "stopped"] {
                let message = socket.next().await.unwrap().unwrap().into_text().unwrap();
                let value: Value = serde_json::from_str(&message).unwrap();
                assert_eq!(value["type"], expected);
            }
        });

        let reconciliations = Arc::new(AtomicUsize::new(0));
        let runtime = DriverRuntime::new(
            format!("http://{address}"),
            "/drivers/example",
            9,
            "secret",
            SlowNoop(reconciliations.clone()),
        );
        assert_eq!(
            runtime.connect_and_serve().await.unwrap(),
            SessionOutcome::Stopped
        );
        server.await.unwrap();
        assert_eq!(reconciliations.load(Ordering::SeqCst), 1);
    }
}
