use std::{collections::HashMap, thread, time::Duration};

use async_trait::async_trait;
use kas_core::{
    LinkSpec, Mutation, PlannedResource, PlannedResourceMetadata, Resource, ResourceStatus,
};
use kas_driver::{Driver, DriverError};
use reqwest::{blocking::Client, StatusCode};
use serde::Deserialize;
use serde_json::{json, Value};

const TELEGRAM_MANIFEST: &str = "/manifests/telegram";
const THREAD_MANIFEST: &str = "/manifests/thread";
const MESSAGE_MANIFEST: &str = "/manifests/message";
const AGENT_MANIFEST: &str = "/manifests/agent";
const USER_MANIFEST: &str = "/builtin/user";
const LINK_MANIFEST: &str = "/builtin/link";
const MESSAGE_THREAD: &str = "/manifests/message/relations/message-thread";
const AUTHORED_BY: &str = "/manifests/message/relations/authored-by";
const REPLIES_TO: &str = "/manifests/message/relations/replies-to";
const MENTIONED: &str = "/manifests/message/relations/mentioned";
const PARTICIPANTS: &str = "/manifests/thread/relations/participants";
const THREAD_TOPIC: &str = "/manifests/telegram/relations/thread-topic";
const MESSAGE_COPY: &str = "/manifests/telegram/relations/message-copy";
const TELEGRAM_IDENTITY: &str = "/manifests/telegram/relations/identity";

#[derive(Debug, Clone)]
pub struct TelegramDriver {
    kas_api: String,
    kas_token: String,
    client: Client,
}

#[derive(Debug, Clone, Deserialize)]
struct TelegramConfig {
    bot_token: String,
    chat_id: String,
    mode: SyncMode,
    #[serde(default = "default_api_base")]
    api_base: String,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
enum SyncMode {
    Bidirectional,
    TelegramToKas,
    KasToTelegram,
}

impl SyncMode {
    fn inbound(self) -> bool {
        matches!(self, Self::Bidirectional | Self::TelegramToKas)
    }

    fn outbound(self) -> bool {
        matches!(self, Self::Bidirectional | Self::KasToTelegram)
    }
}

#[derive(Debug, Deserialize)]
struct TelegramResponse<T> {
    ok: bool,
    result: Option<T>,
    description: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TelegramUpdate {
    update_id: i64,
    message: Option<TelegramMessage>,
    edited_message: Option<TelegramMessage>,
}

#[derive(Debug, Clone, Deserialize)]
struct TelegramMessage {
    message_id: i64,
    message_thread_id: Option<i64>,
    chat: TelegramChat,
    from: Option<TelegramUser>,
    text: Option<String>,
    caption: Option<String>,
    reply_to_message: Option<Box<TelegramMessage>>,
    forum_topic_edited: Option<ForumTopicEdited>,
}

#[derive(Debug, Clone, Deserialize)]
struct TelegramChat {
    id: i64,
}

#[derive(Debug, Clone, Deserialize)]
struct TelegramUser {
    id: i64,
    #[serde(default)]
    is_bot: bool,
    first_name: String,
    last_name: Option<String>,
    username: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct ForumTopicEdited {
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TelegramForumTopic {
    message_thread_id: i64,
    name: String,
}

impl TelegramDriver {
    pub fn new(kas_api: impl Into<String>, kas_token: impl Into<String>) -> Self {
        Self {
            kas_api: kas_api.into().trim_end_matches('/').to_owned(),
            kas_token: kas_token.into(),
            client: Client::builder()
                .timeout(Duration::from_secs(35))
                .build()
                .expect("Telegram HTTP client configuration is valid"),
        }
    }

    pub fn poll_forever(&self) {
        let mut offsets = HashMap::<String, i64>::new();
        loop {
            match self.list_resources(TELEGRAM_MANIFEST) {
                Ok(configurations) if configurations.is_empty() => {
                    thread::sleep(Duration::from_secs(1));
                }
                Ok(configurations) => {
                    for resource in configurations {
                        let Ok(config) = decode_config(&resource) else {
                            continue;
                        };
                        if !config.mode.inbound() {
                            continue;
                        }
                        let offset = offsets.get(&resource.path).copied();
                        match self.poll_configuration(&resource, &config, offset) {
                            Ok(next) => {
                                if let Some(next) = next {
                                    offsets.insert(resource.path.clone(), next);
                                }
                            }
                            Err(error) => {
                                eprintln!("Telegram polling failed for {}: {error}", resource.path);
                                thread::sleep(Duration::from_secs(2));
                            }
                        }
                    }
                }
                Err(error) => {
                    eprintln!("Telegram Driver could not load configurations: {error}");
                    thread::sleep(Duration::from_secs(2));
                }
            }
        }
    }

    fn poll_configuration(
        &self,
        resource: &Resource,
        config: &TelegramConfig,
        offset: Option<i64>,
    ) -> Result<Option<i64>, String> {
        let mut request = json!({
            "timeout": 20,
            "allowed_updates": ["message", "edited_message"]
        });
        if let Some(offset) = offset {
            request["offset"] = offset.into();
        }
        let updates: Vec<TelegramUpdate> = self.telegram_call(config, "getUpdates", request)?;
        let mut next = offset;
        for update in updates {
            self.import_update(resource, config, &update)?;
            next = Some(update.update_id + 1);
        }
        Ok(next)
    }

    fn import_update(
        &self,
        configuration: &Resource,
        config: &TelegramConfig,
        update: &TelegramUpdate,
    ) -> Result<(), String> {
        let (message, edited) = if let Some(message) = &update.message {
            (message, false)
        } else if let Some(message) = &update.edited_message {
            (message, true)
        } else {
            return Ok(());
        };
        if message.chat.id.to_string() != config.chat_id {
            return Ok(());
        }
        if message.from.as_ref().is_some_and(|sender| sender.is_bot) {
            return Ok(());
        }

        let Some(topic_id) = message.message_thread_id else {
            return Ok(());
        };
        let Some(thread_resource) = self.managed_topic(configuration, topic_id)? else {
            return Ok(());
        };
        if let Some(actual_name) = message
            .forum_topic_edited
            .as_ref()
            .and_then(|edited| edited.name.as_deref())
        {
            let expected_name = thread_title(&thread_resource);
            if actual_name != expected_name {
                let _: bool = self.telegram_call(
                    config,
                    "editForumTopic",
                    json!({
                        "chat_id": config.chat_id,
                        "message_thread_id": topic_id,
                        "name": expected_name
                    }),
                )?;
            }
        }
        let body = message
            .text
            .as_deref()
            .or(message.caption.as_deref())
            .unwrap_or("")
            .trim();
        if body.is_empty() {
            return Ok(());
        }

        let message_path = telegram_message_path(configuration, message.message_id);
        if let Some(existing) = self.get_resource(&message_path)? {
            if edited && existing.spec.get("body").and_then(Value::as_str) != Some(body) {
                self.update_message_body(&existing, body)?;
            }
            return Ok(());
        }

        let sender = message
            .from
            .as_ref()
            .ok_or_else(|| "Telegram Message has no sender".to_owned())?;
        let user_path = format!("/users/telegram/{}", sender.id);
        self.ensure_resource(json!({
            "path": user_path,
            "metadata": {
                "manifest": USER_MANIFEST,
                "name": telegram_user_name(sender)
            },
            "spec": {
                "disabled": false
            }
        }))?;
        self.ensure_link(
            &format!(
                "{user_path}/links/telegram/{}",
                path_slug(&configuration.path)
            ),
            TELEGRAM_IDENTITY,
            &user_path,
            &configuration.path,
            json!({
                "user_id": sender.id,
                "username": sender.username.clone().unwrap_or_default()
            }),
        )?;
        self.ensure_participant(&thread_resource.path, &user_path)?;

        self.ensure_resource(json!({
            "path": message_path,
            "metadata": {
                "manifest": MESSAGE_MANIFEST,
                "name": format!("Telegram {}", message.message_id)
            },
            "spec": {
                "role": "user",
                "body": body
            }
        }))?;
        self.ensure_link(
            &format!(
                "{message_path}/links/telegram/{}",
                path_slug(&configuration.path)
            ),
            MESSAGE_COPY,
            &message_path,
            &configuration.path,
            json!({
                "direction": "telegram-to-kas",
                "message_ids": [message.message_id],
                "update_id": update.update_id
            }),
        )?;
        self.ensure_link(
            &format!("{message_path}/links/thread"),
            MESSAGE_THREAD,
            &message_path,
            &thread_resource.path,
            json!({}),
        )?;
        self.ensure_link(
            &format!("{message_path}/links/author"),
            AUTHORED_BY,
            &message_path,
            &user_path,
            json!({}),
        )?;

        if let Some(reply) = &message.reply_to_message {
            let target = telegram_message_path(configuration, reply.message_id);
            if self.get_resource(&target)?.is_some() {
                self.ensure_link(
                    &format!("{message_path}/links/reply"),
                    REPLIES_TO,
                    &message_path,
                    &target,
                    json!({}),
                )?;
            }
        }

        for agent in self.list_resources(AGENT_MANIFEST)? {
            let handle = agent
                .path
                .split('/')
                .rfind(|part| !part.is_empty())
                .unwrap_or(&agent.metadata.name);
            if !contains_mention(body, handle) {
                continue;
            }
            self.ensure_participant(&thread_resource.path, &agent.path)?;
            self.ensure_link(
                &format!("{message_path}/links/mentioned/{}", path_slug(&agent.path)),
                MENTIONED,
                &message_path,
                &agent.path,
                json!({}),
            )?;
        }
        Ok(())
    }

    fn managed_topic(
        &self,
        configuration: &Resource,
        topic_id: i64,
    ) -> Result<Option<Resource>, String> {
        for link_resource in self.list_links()? {
            let Ok(link) = serde_json::from_value::<LinkSpec>(link_resource.spec.clone()) else {
                continue;
            };
            if link.relation == THREAD_TOPIC
                && link.target == configuration.path
                && topic_managed(&link)
                && link.metadata.get("topic_id").and_then(Value::as_i64) == Some(topic_id)
            {
                let thread = self.get_resource(&link.source)?.ok_or_else(|| {
                    format!("Telegram Topic Link {} is dangling", link_resource.path)
                })?;
                return Ok(Some(thread));
            }
        }
        Ok(None)
    }

    fn ensure_participant(&self, thread_path: &str, participant: &str) -> Result<(), String> {
        self.ensure_link(
            &format!(
                "{thread_path}/links/participants/{}",
                path_slug(participant)
            ),
            PARTICIPANTS,
            thread_path,
            participant,
            json!({}),
        )
    }

    fn update_message_body(&self, resource: &Resource, body: &str) -> Result<(), String> {
        let response = self
            .client
            .patch(format!("{}/resources/by-path", self.kas_api))
            .bearer_auth(&self.kas_token)
            .query(&[("path", resource.path.as_str())])
            .json(&json!({
                "expected_revision": resource.revision,
                "spec": {
                    "role": "user",
                    "body": body
                }
            }))
            .send()
            .map_err(|error| format!("could not update {}: {error}", resource.path))?;
        response
            .error_for_status()
            .map(|_| ())
            .map_err(|error| format!("could not update {}: {error}", resource.path))
    }

    fn ensure_resource(&self, resource: Value) -> Result<(), String> {
        let path = resource
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| "Resource path is missing".to_owned())?;
        if self.get_resource(path)?.is_some() {
            return Ok(());
        }
        let response = self
            .client
            .post(format!("{}/resources", self.kas_api))
            .bearer_auth(&self.kas_token)
            .json(&resource)
            .send()
            .map_err(|error| format!("could not create {path}: {error}"))?;
        if response.status().is_success() || response.status() == StatusCode::CONFLICT {
            return Ok(());
        }
        Err(format!(
            "could not create {path}: {}",
            response.text().unwrap_or_default()
        ))
    }

    fn ensure_link(
        &self,
        path: &str,
        relation: &str,
        source: &str,
        target: &str,
        metadata: Value,
    ) -> Result<(), String> {
        self.ensure_resource(json!({
            "path": path,
            "metadata": {
                "manifest": LINK_MANIFEST,
                "name": path.rsplit('/').next().unwrap_or("link")
            },
            "spec": {
                "relation": relation,
                "source": source,
                "target": target,
                "metadata": metadata
            }
        }))
    }

    fn get_resource(&self, path: &str) -> Result<Option<Resource>, String> {
        let response = self
            .client
            .get(format!("{}/resources/by-path", self.kas_api))
            .bearer_auth(&self.kas_token)
            .query(&[("path", path)])
            .send()
            .map_err(|error| format!("could not load {path}: {error}"))?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        response
            .error_for_status()
            .and_then(reqwest::blocking::Response::json)
            .map(Some)
            .map_err(|error| format!("could not load {path}: {error}"))
    }

    fn list_resources(&self, manifest: &str) -> Result<Vec<Resource>, String> {
        self.client
            .get(format!("{}/resources", self.kas_api))
            .bearer_auth(&self.kas_token)
            .query(&[("manifest", manifest)])
            .send()
            .and_then(reqwest::blocking::Response::error_for_status)
            .and_then(reqwest::blocking::Response::json)
            .map_err(|error| format!("could not list {manifest}: {error}"))
    }

    fn list_links(&self) -> Result<Vec<Resource>, String> {
        self.list_resources(LINK_MANIFEST)
    }

    fn telegram_call<T: for<'de> Deserialize<'de>>(
        &self,
        config: &TelegramConfig,
        method: &str,
        body: Value,
    ) -> Result<T, String> {
        let base = config.api_base.trim_end_matches('/');
        let response = self
            .client
            .post(format!("{base}/bot{}/{method}", config.bot_token))
            .json(&body)
            .send()
            .map_err(|error| format!("Telegram {method} failed: {error}"))?;
        let status = response.status();
        let response: TelegramResponse<T> = response.json().map_err(|error| {
            format!("Telegram {method} returned invalid JSON ({status}): {error}")
        })?;
        if !response.ok {
            let description = response
                .description
                .unwrap_or_else(|| "ok=false without a description".into());
            return Err(format!(
                "Telegram {method} failed ({status}): {description}"
            ));
        }
        response
            .result
            .ok_or_else(|| format!("Telegram {method} omitted result"))
    }

    fn reconcile_topic_link(
        &self,
        resource: &Resource,
        link: LinkSpec,
    ) -> Result<Vec<Mutation>, String> {
        if link.relation != THREAD_TOPIC || !topic_managed(&link) {
            return Ok(Vec::new());
        }
        let Some(configuration) = self.get_resource(&link.target)? else {
            return Ok(Vec::new());
        };
        let config = decode_config(&configuration)?;
        let topic_id = link.metadata.get("topic_id").and_then(Value::as_i64);
        if resource.metadata.state == kas_core::STATE_DELETED {
            if let Some(topic_id) = topic_id {
                let closed = self.telegram_call::<bool>(
                    &config,
                    "closeForumTopic",
                    json!({
                        "chat_id": config.chat_id,
                        "message_thread_id": topic_id
                    }),
                );
                if let Err(error) = closed {
                    let already_closed = error.to_ascii_lowercase().contains("already closed");
                    if !already_closed {
                        return Err(error);
                    }
                }
            }
            return Ok(Vec::new());
        }
        let Some(thread) = self.get_resource(&link.source)? else {
            return Ok(Vec::new());
        };
        if thread.manifest != THREAD_MANIFEST || thread.metadata.state == kas_core::STATE_DELETED {
            return Ok(Vec::new());
        }
        let desired_name = thread_title(&thread);
        let (topic_id, actual_name) = if let Some(topic_id) = topic_id {
            let current_name = link
                .metadata
                .get("topic_name")
                .and_then(Value::as_str)
                .unwrap_or("");
            if current_name == desired_name {
                return Ok(Vec::new());
            }
            let _: bool = self.telegram_call(
                &config,
                "editForumTopic",
                json!({
                    "chat_id": config.chat_id,
                    "message_thread_id": topic_id,
                    "name": desired_name
                }),
            )?;
            (topic_id, desired_name)
        } else {
            let topic: TelegramForumTopic = self.telegram_call(
                &config,
                "createForumTopic",
                json!({
                    "chat_id": config.chat_id,
                    "name": desired_name
                }),
            )?;
            (topic.message_thread_id, topic.name)
        };
        let mut metadata = link.metadata.as_object().cloned().unwrap_or_default();
        metadata.insert("managed".into(), Value::Bool(true));
        metadata.insert("topic_id".into(), topic_id.into());
        metadata.insert("topic_name".into(), actual_name.into());
        Ok(vec![Mutation::UpdateResource {
            resource_path: resource.path.clone(),
            expected_revision: resource.revision,
            metadata: None,
            spec: serde_json::to_value(LinkSpec {
                relation: link.relation,
                source: link.source,
                target: link.target,
                metadata: Value::Object(metadata),
            })
            .expect("LinkSpec is serializable"),
        }])
    }

    fn reconcile_thread(&self, thread: &Resource) -> Result<Vec<Mutation>, String> {
        if thread.metadata.state == kas_core::STATE_DELETED {
            return Ok(Vec::new());
        }
        let mut mutations = Vec::new();
        for resource in self.list_links()? {
            let Ok(link) = serde_json::from_value::<LinkSpec>(resource.spec.clone()) else {
                continue;
            };
            if link.relation == THREAD_TOPIC && link.source == thread.path && topic_managed(&link) {
                mutations.extend(self.reconcile_topic_link(&resource, link)?);
            }
        }
        Ok(mutations)
    }

    fn reconcile_blocking(&self, resource: &Resource) -> Result<Vec<Mutation>, String> {
        if resource.manifest == TELEGRAM_MANIFEST {
            let config = decode_config(resource)?;
            let _: Value = self.telegram_call(&config, "getMe", json!({}))?;
            return Ok(vec![Mutation::UpdateResourceStatus {
                resource_path: resource.path.clone(),
                expected_revision: resource.revision,
                status: ResourceStatus {
                    metadata: resource.status_metadata(resource.metadata.state.clone()),
                    spec: resource.spec.clone(),
                },
            }]);
        }
        if resource.manifest == LINK_MANIFEST {
            let link = serde_json::from_value::<LinkSpec>(resource.spec.clone())
                .map_err(|error| format!("invalid Link {}: {error}", resource.path))?;
            if link.relation == THREAD_TOPIC {
                return self.reconcile_topic_link(resource, link);
            }
            if link.relation == MESSAGE_THREAD {
                return match self.get_resource(&link.source)? {
                    Some(message) if message.manifest == MESSAGE_MANIFEST => {
                        self.reconcile_blocking(&message)
                    }
                    _ => Ok(Vec::new()),
                };
            }
            return Ok(Vec::new());
        }
        if resource.manifest == THREAD_MANIFEST {
            return self.reconcile_thread(resource);
        }
        if resource.manifest != MESSAGE_MANIFEST {
            return Ok(Vec::new());
        }
        let links = self.list_links()?;
        let Some(thread_path) = link_target(&links, MESSAGE_THREAD, &resource.path) else {
            return Ok(Vec::new());
        };
        let mut mutations = Vec::new();
        for topic_resource in &links {
            let Ok(topic) = serde_json::from_value::<LinkSpec>(topic_resource.spec.clone()) else {
                continue;
            };
            if topic.relation != THREAD_TOPIC
                || topic.source != thread_path
                || !topic_managed(&topic)
            {
                continue;
            }
            if has_link(&links, MESSAGE_COPY, &resource.path, &topic.target) {
                continue;
            }
            let Some(configuration) = self.get_resource(&topic.target)? else {
                continue;
            };
            let config = decode_config(&configuration)?;
            if !config.mode.outbound() {
                continue;
            }
            let Some(topic_id) = topic.metadata.get("topic_id").and_then(Value::as_i64) else {
                continue;
            };
            let mut text = resource
                .spec
                .get("body")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            if let Some(author_path) = link_target(&links, AUTHORED_BY, &resource.path) {
                if author_path.starts_with("/agents/") {
                    let handle = author_path
                        .split('/')
                        .rfind(|part| !part.is_empty())
                        .unwrap_or("agent");
                    text = format!("@{handle}\n\n{text}");
                }
            }
            if text.trim().is_empty() {
                continue;
            }
            let reply_message_id =
                link_target(&links, REPLIES_TO, &resource.path).and_then(|target| {
                    links.iter().find_map(|link_resource| {
                        let link =
                            serde_json::from_value::<LinkSpec>(link_resource.spec.clone()).ok()?;
                        (link.relation == MESSAGE_COPY
                            && link.source == target
                            && link.target == configuration.path)
                            .then(|| {
                                link.metadata
                                    .get("message_ids")
                                    .and_then(Value::as_array)
                                    .and_then(|ids| ids.last())
                                    .and_then(Value::as_i64)
                            })
                            .flatten()
                    })
                });
            let mut message_ids = Vec::new();
            for chunk in split_telegram_text(&text) {
                let mut request = json!({
                    "chat_id": config.chat_id,
                    "message_thread_id": topic_id,
                    "text": chunk
                });
                if let Some(reply) = reply_message_id {
                    request["reply_parameters"] = json!({ "message_id": reply });
                }
                let sent: TelegramMessage = self.telegram_call(&config, "sendMessage", request)?;
                message_ids.push(sent.message_id);
            }
            mutations.push(Mutation::CreateResource {
                resource: planned_link(
                    format!(
                        "{}/links/telegram/{}",
                        resource.path,
                        path_slug(&configuration.path)
                    ),
                    MESSAGE_COPY,
                    resource.path.clone(),
                    configuration.path,
                    json!({
                        "direction": "kas-to-telegram",
                        "message_ids": message_ids
                    }),
                ),
            });
        }
        Ok(mutations)
    }
}

#[async_trait]
impl Driver for TelegramDriver {
    fn name(&self) -> &str {
        "telegram"
    }

    async fn reconcile(&self, resource: &Resource) -> Result<Vec<Mutation>, DriverError> {
        let driver = self.clone();
        let resource = resource.clone();
        thread::spawn(move || driver.reconcile_blocking(&resource))
            .join()
            .map_err(|_| execution_error("Telegram reconciliation worker panicked"))?
            .map_err(execution_error)
    }

    async fn execute(
        &self,
        _resource: &Resource,
        action: &Resource,
        _run: &Resource,
    ) -> Result<kas_core::DriverExecution, DriverError> {
        Err(DriverError::UnsupportedAction(action.path.clone()))
    }
}

fn decode_config(resource: &Resource) -> Result<TelegramConfig, String> {
    serde_json::from_value(resource.spec.clone())
        .map_err(|error| format!("invalid Telegram configuration {}: {error}", resource.path))
}

fn default_api_base() -> String {
    "https://api.telegram.org".into()
}

fn telegram_message_path(configuration: &Resource, message_id: i64) -> String {
    format!(
        "/messages/telegram/{}/{}",
        path_slug(&configuration.path),
        message_id
    )
}

fn telegram_user_name(user: &TelegramUser) -> String {
    let mut name = user.first_name.clone();
    if let Some(last) = &user.last_name {
        if !last.trim().is_empty() {
            name.push(' ');
            name.push_str(last);
        }
    }
    if name.trim().is_empty() {
        user.username
            .clone()
            .unwrap_or_else(|| format!("Telegram {}", user.id))
    } else {
        name
    }
}

fn thread_title(resource: &Resource) -> String {
    resource
        .spec
        .get("title")
        .and_then(Value::as_str)
        .filter(|title| !title.trim().is_empty())
        .unwrap_or(&resource.metadata.name)
        .to_owned()
}

fn topic_managed(link: &LinkSpec) -> bool {
    link.metadata.get("managed").and_then(Value::as_bool) == Some(true)
}

fn link_target(links: &[Resource], relation: &str, source: &str) -> Option<String> {
    links.iter().find_map(|resource| {
        let link: LinkSpec = serde_json::from_value(resource.spec.clone()).ok()?;
        (link.relation == relation && link.source == source).then_some(link.target)
    })
}

fn has_link(links: &[Resource], relation: &str, source: &str, target: &str) -> bool {
    links.iter().any(|resource| {
        serde_json::from_value::<LinkSpec>(resource.spec.clone()).is_ok_and(|link| {
            link.relation == relation && link.source == source && link.target == target
        })
    })
}

fn planned_link(
    path: String,
    relation: impl Into<String>,
    source: String,
    target: String,
    metadata: Value,
) -> PlannedResource {
    let name = path.rsplit('/').next().unwrap_or("link").to_owned();
    PlannedResource {
        path,
        metadata: PlannedResourceMetadata {
            manifest: LINK_MANIFEST.into(),
            name,
            state: String::new(),
        },
        spec: serde_json::to_value(LinkSpec {
            relation: relation.into(),
            source,
            target,
            metadata,
        })
        .expect("LinkSpec is serializable"),
        status: ResourceStatus::default(),
    }
}

fn path_slug(path: &str) -> String {
    path.trim_matches('/')
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_owned()
}

fn contains_mention(body: &str, handle: &str) -> bool {
    let body = body.to_ascii_lowercase();
    let needle = format!("@{}", handle.to_ascii_lowercase());
    body.match_indices(&needle).any(|(index, _)| {
        let before = body[..index].chars().next_back();
        let after = body[index + needle.len()..].chars().next();
        before.is_none_or(|character| character.is_whitespace())
            && after.is_none_or(|character| {
                character.is_whitespace() || ",.!?;:()[]{}".contains(character)
            })
    })
}

fn split_telegram_text(text: &str) -> Vec<String> {
    const MAX: usize = 4096;
    if text.chars().count() <= MAX {
        return vec![text.to_owned()];
    }
    let mut chunks = Vec::new();
    let mut current = String::new();
    for character in text.chars() {
        if current.chars().count() == MAX {
            chunks.push(std::mem::take(&mut current));
        }
        current.push(character);
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

fn execution_error(message: impl Into<String>) -> DriverError {
    DriverError::Execution(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mention_does_not_require_bot_username() {
        assert!(contains_mention(
            "please ask @reviewer to check",
            "reviewer"
        ));
        assert!(contains_mention("@reviewer, check this", "reviewer"));
        assert!(!contains_mention("mail@reviewer.example", "reviewer"));
        assert!(!contains_mention("@reviewer-extra", "reviewer"));
    }

    #[test]
    fn telegram_text_is_split_on_character_boundaries() {
        let text = "好".repeat(5000);
        let chunks = split_telegram_text(&text);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].chars().count(), 4096);
        assert_eq!(chunks[1].chars().count(), 904);
    }
}
