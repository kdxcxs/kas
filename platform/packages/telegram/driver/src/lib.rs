use std::{collections::HashMap, thread, time::Duration};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use kas_core::{
    LinkSpec, Mutation, PlannedResource, PlannedResourceMetadata, Resource, ResourceStatus,
};
use kas_driver::{Driver, DriverError};
use reqwest::{
    blocking::{multipart, Client},
    StatusCode,
};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use uuid::Uuid;

const TELEGRAM_MANIFEST: &str = "/manifests/telegram";
const APPROVAL_MANIFEST: &str = "/manifests/approval";
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
const FILE_MANIFEST: &str = "/manifests/file";
const ATTACHED_TO: &str = "/manifests/file/relations/attached-to";
const TELEGRAM_IDENTITY: &str = "/manifests/telegram/relations/identity";
const BINDING_REQUEST: &str = "/manifests/telegram/relations/binding-request";
const USER_BINDING: &str = "/manifests/telegram/relations/user-binding";
const APPROVAL_DELIVERY: &str = "/manifests/telegram/relations/approval-delivery";

#[derive(Debug, Clone)]
pub struct TelegramDriver {
    kas_api: String,
    file_api: String,
    approval_api: String,
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
    #[serde(default)]
    bot_username: Option<String>,
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
    callback_query: Option<TelegramCallbackQuery>,
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
struct TelegramCallbackQuery {
    id: String,
    from: TelegramUser,
    message: Option<TelegramMessage>,
    data: Option<String>,
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

#[derive(Debug, Deserialize)]
struct IssuedCredential {
    resource_path: String,
    token: String,
}

#[derive(Debug, Deserialize)]
struct FileSpec {
    filename: String,
    media_type: String,
}

impl TelegramDriver {
    pub fn new(kas_api: impl Into<String>, kas_token: impl Into<String>) -> Self {
        Self {
            kas_api: kas_api.into().trim_end_matches('/').to_owned(),
            file_api: "http://127.0.0.1:3001".into(),
            approval_api: "http://127.0.0.1:3003".into(),
            kas_token: kas_token.into(),
            client: Client::builder()
                .timeout(Duration::from_secs(35))
                .build()
                .expect("Telegram HTTP client configuration is valid"),
        }
    }

    pub fn with_file_api(mut self, file_api: impl Into<String>) -> Self {
        self.file_api = file_api.into().trim_end_matches('/').to_owned();
        self
    }

    pub fn with_approval_api(mut self, approval_api: impl Into<String>) -> Self {
        self.approval_api = approval_api.into().trim_end_matches('/').to_owned();
        self
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
                        if config.mode.outbound() {
                            if let Err(error) = self.deliver_pending_approvals(&resource, &config) {
                                eprintln!(
                                    "Telegram Approval delivery failed for {}: {error}",
                                    resource.path
                                );
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
            "allowed_updates": ["message", "edited_message", "callback_query"]
        });
        if let Some(offset) = offset {
            request["offset"] = offset.into();
        }
        let updates: Vec<TelegramUpdate> = self.telegram_call(config, "getUpdates", request)?;
        let mut next = offset;
        for update in updates {
            if let Some(callback) = &update.callback_query {
                if let Err(error) = self.handle_approval_callback(resource, config, callback) {
                    eprintln!("Telegram Approval callback failed: {error}");
                    let _ = self.answer_callback(
                        config,
                        &callback.id,
                        "Approval failed. Open KAS for details.",
                        true,
                    );
                }
            } else {
                self.import_update(resource, config, &update)?;
            }
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
        if let Some(sender) = &message.from {
            let text = message.text.as_deref().unwrap_or("").trim();
            if message.chat.id == sender.id && text.starts_with("/start") {
                return self.handle_binding_command(configuration, config, message, text);
            }
        }
        if message.chat.id.to_string() != config.chat_id {
            return Ok(());
        }
        if !config.mode.inbound() {
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
        let author_path = self
            .bound_kas_user(configuration, &user_path)?
            .unwrap_or_else(|| user_path.clone());
        self.ensure_participant(&thread_resource.path, &author_path)?;

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
            &author_path,
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

    fn handle_binding_command(
        &self,
        configuration: &Resource,
        config: &TelegramConfig,
        message: &TelegramMessage,
        text: &str,
    ) -> Result<(), String> {
        let Some(token) = text.split_whitespace().nth(1) else {
            self.send_private_text(
                config,
                message.chat.id,
                "Open the binding link generated by KAS to connect this Telegram account.",
            )?;
            return Ok(());
        };
        let sender = message
            .from
            .as_ref()
            .ok_or_else(|| "Telegram binding command has no sender".to_owned())?;
        let token_hash = hash_token(token);
        let challenge = self.list_links()?.into_iter().find(|resource| {
            if resource.metadata.state == kas_core::STATE_DELETED {
                return false;
            }
            let Ok(link) = serde_json::from_value::<LinkSpec>(resource.spec.clone()) else {
                return false;
            };
            if link.relation != BINDING_REQUEST || link.target != configuration.path {
                return false;
            }
            let expires_at = link
                .metadata
                .get("expires_at")
                .and_then(Value::as_str)
                .and_then(|value| DateTime::parse_from_rfc3339(value).ok());
            link.metadata.get("token_hash").and_then(Value::as_str) == Some(&token_hash)
                && expires_at.is_some_and(|value| value.with_timezone(&Utc) > Utc::now())
        });
        let Some(challenge) = challenge else {
            self.send_private_text(
                config,
                message.chat.id,
                "This KAS binding link is invalid or expired. Generate a new link in KAS.",
            )?;
            return Ok(());
        };
        let challenge_link: LinkSpec = serde_json::from_value(challenge.spec.clone())
            .map_err(|error| format!("invalid binding challenge {}: {error}", challenge.path))?;
        if challenge_link.source.starts_with("/users/telegram/") {
            return Err("a Telegram shadow User cannot initiate a KAS binding".into());
        }
        let kas_user = self
            .get_resource(&challenge_link.source)?
            .ok_or_else(|| format!("KAS User {} no longer exists", challenge_link.source))?;
        if kas_user.manifest != USER_MANIFEST {
            return Err(format!("{} is not a KAS User", kas_user.path));
        }

        let telegram_user_path = format!("/users/telegram/{}", sender.id);
        self.ensure_resource(json!({
            "path": telegram_user_path,
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
                "{telegram_user_path}/links/telegram/{}",
                path_slug(&configuration.path)
            ),
            TELEGRAM_IDENTITY,
            &telegram_user_path,
            &configuration.path,
            json!({
                "user_id": sender.id,
                "username": sender.username.clone().unwrap_or_default()
            }),
        )?;

        let links = self.list_links()?;
        if let Some(existing) = links.iter().find_map(|resource| {
            let link = serde_json::from_value::<LinkSpec>(resource.spec.clone()).ok()?;
            (resource.metadata.state != kas_core::STATE_DELETED
                && link.relation == USER_BINDING
                && link.metadata.get("configuration").and_then(Value::as_str)
                    == Some(configuration.path.as_str())
                && (link.source == kas_user.path || link.target == telegram_user_path))
                .then_some((resource, link))
        }) {
            if existing.1.source != kas_user.path || existing.1.target != telegram_user_path {
                self.send_private_text(
                    config,
                    message.chat.id,
                    "This KAS User or Telegram account is already bound. Unbind it in KAS first.",
                )?;
                return Ok(());
            }
        } else {
            self.ensure_link(
                &format!(
                    "{}/links/telegram/{}",
                    kas_user.path,
                    path_slug(&configuration.path)
                ),
                USER_BINDING,
                &kas_user.path,
                &telegram_user_path,
                json!({
                    "configuration": configuration.path,
                    "user_id": sender.id,
                    "private_chat_id": message.chat.id,
                    "username": sender.username.clone().unwrap_or_default(),
                    "bound_at": Utc::now().to_rfc3339()
                }),
            )?;
        }
        self.delete_resource(&challenge)?;
        self.send_private_text(
            config,
            message.chat.id,
            &format!(
                "Telegram is now bound to KAS User {}. Approval requests can be handled here.",
                kas_user.path
            ),
        )
    }

    fn bound_kas_user(
        &self,
        configuration: &Resource,
        telegram_user_path: &str,
    ) -> Result<Option<String>, String> {
        Ok(self.list_links()?.into_iter().find_map(|resource| {
            let link = serde_json::from_value::<LinkSpec>(resource.spec).ok()?;
            (resource.metadata.state != kas_core::STATE_DELETED
                && link.relation == USER_BINDING
                && link.target == telegram_user_path
                && link.metadata.get("configuration").and_then(Value::as_str)
                    == Some(configuration.path.as_str()))
            .then_some(link.source)
        }))
    }

    fn deliver_pending_approvals(
        &self,
        configuration: &Resource,
        config: &TelegramConfig,
    ) -> Result<(), String> {
        let links = self.list_links()?;
        let bindings = links
            .iter()
            .filter_map(|resource| {
                let link = serde_json::from_value::<LinkSpec>(resource.spec.clone()).ok()?;
                (resource.metadata.state != kas_core::STATE_DELETED
                    && link.relation == USER_BINDING
                    && link.metadata.get("configuration").and_then(Value::as_str)
                        == Some(configuration.path.as_str()))
                .then_some(link)
            })
            .collect::<Vec<_>>();
        if bindings.is_empty() {
            return Ok(());
        }
        for approval in self.list_resources(APPROVAL_MANIFEST)? {
            if approval.metadata.state != "pending"
                || approval.spec.get("kind").and_then(Value::as_str) != Some("request")
            {
                continue;
            }
            let expires_at = approval
                .spec
                .get("expires_at")
                .and_then(Value::as_str)
                .and_then(|value| DateTime::parse_from_rfc3339(value).ok());
            if expires_at.is_some_and(|value| value.with_timezone(&Utc) <= Utc::now()) {
                continue;
            }
            for binding in &bindings {
                if links.iter().any(|resource| {
                    serde_json::from_value::<LinkSpec>(resource.spec.clone()).is_ok_and(|link| {
                        resource.metadata.state != kas_core::STATE_DELETED
                            && link.relation == APPROVAL_DELIVERY
                            && link.source == approval.path
                            && link.target == binding.source
                            && link.metadata.get("configuration").and_then(Value::as_str)
                                == Some(configuration.path.as_str())
                    })
                }) {
                    continue;
                }
                let Some(chat_id) = binding
                    .metadata
                    .get("private_chat_id")
                    .and_then(Value::as_i64)
                else {
                    continue;
                };
                let Some(telegram_user_id) =
                    binding.metadata.get("user_id").and_then(Value::as_i64)
                else {
                    continue;
                };
                let callback_token = Uuid::new_v4().simple().to_string();
                let sent: TelegramMessage = self.telegram_call(
                    config,
                    "sendMessage",
                    json!({
                        "chat_id": chat_id,
                        "text": approval_message(&approval),
                        "reply_markup": {
                            "inline_keyboard": [[
                                {
                                    "text": "Approve",
                                    "callback_data": format!(
                                        "kas-approval:approve:{callback_token}"
                                    )
                                },
                                {
                                    "text": "Reject",
                                    "callback_data": format!(
                                        "kas-approval:reject:{callback_token}"
                                    )
                                }
                            ]]
                        }
                    }),
                )?;
                self.ensure_link(
                    &format!(
                        "{}/links/telegram/{}-{}",
                        approval.path,
                        path_slug(&binding.source),
                        path_slug(&configuration.path)
                    ),
                    APPROVAL_DELIVERY,
                    &approval.path,
                    &binding.source,
                    json!({
                        "configuration": configuration.path,
                        "telegram_user_id": telegram_user_id,
                        "private_chat_id": chat_id,
                        "callback_token_hash": hash_token(&callback_token),
                        "message_id": sent.message_id
                    }),
                )?;
            }
        }
        Ok(())
    }

    fn handle_approval_callback(
        &self,
        configuration: &Resource,
        config: &TelegramConfig,
        callback: &TelegramCallbackQuery,
    ) -> Result<(), String> {
        let Some(data) = callback.data.as_deref() else {
            return Ok(());
        };
        let mut parts = data.split(':');
        if parts.next() != Some("kas-approval") {
            return Ok(());
        }
        let decision = parts
            .next()
            .filter(|value| matches!(*value, "approve" | "reject"))
            .ok_or_else(|| "Telegram Approval callback has an invalid decision".to_owned())?;
        let token = parts
            .next()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| "Telegram Approval callback has no token".to_owned())?;
        if parts.next().is_some() {
            return Err("Telegram Approval callback has extra data".into());
        }
        self.answer_callback(config, &callback.id, "Processing in KAS…", false)?;
        let token_hash = hash_token(token);
        let delivery = self.list_links()?.into_iter().find(|resource| {
            let Ok(link) = serde_json::from_value::<LinkSpec>(resource.spec.clone()) else {
                return false;
            };
            resource.metadata.state != kas_core::STATE_DELETED
                && link.relation == APPROVAL_DELIVERY
                && link.metadata.get("configuration").and_then(Value::as_str)
                    == Some(configuration.path.as_str())
                && link
                    .metadata
                    .get("callback_token_hash")
                    .and_then(Value::as_str)
                    == Some(token_hash.as_str())
        });
        let delivery =
            delivery.ok_or_else(|| "Telegram Approval callback is invalid or stale".to_owned())?;
        let delivery_link: LinkSpec = serde_json::from_value(delivery.spec.clone())
            .map_err(|error| format!("invalid Approval delivery {}: {error}", delivery.path))?;
        let callback_message = callback
            .message
            .as_ref()
            .ok_or_else(|| "Telegram Approval callback has no Message".to_owned())?;
        let expected_user = delivery_link
            .metadata
            .get("telegram_user_id")
            .and_then(Value::as_i64);
        let expected_chat = delivery_link
            .metadata
            .get("private_chat_id")
            .and_then(Value::as_i64);
        let expected_message = delivery_link
            .metadata
            .get("message_id")
            .and_then(Value::as_i64);
        if expected_user != Some(callback.from.id)
            || expected_chat != Some(callback_message.chat.id)
            || expected_message != Some(callback_message.message_id)
        {
            return Err("Telegram Approval callback identity does not match its delivery".into());
        }
        let approval = self
            .get_resource(&delivery_link.source)?
            .ok_or_else(|| "Approval Request no longer exists".to_owned())?;
        if approval.metadata.state != "pending" {
            self.edit_approval_message(
                config,
                callback_message,
                &format!("Already {}", approval.metadata.state),
            )?;
            return Ok(());
        }
        let credential = self.issue_user_credential(&delivery_link.target)?;
        let decided = (|| {
            let approval_revision = approval.revision.to_string();
            let response = self
                .client
                .post(format!("{}/approvals/decide", self.approval_api))
                .bearer_auth(&credential.token)
                .query(&[
                    ("path", approval.path.as_str()),
                    ("expected_revision", approval_revision.as_str()),
                ])
                .json(&json!({ "decision": decision }))
                .send()
                .map_err(|error| format!("could not decide Approval {}: {error}", approval.path))?;
            let status = response.status();
            if !status.is_success() {
                return Err(format!(
                    "Approval service rejected Telegram decision ({status}): {}",
                    response.text().unwrap_or_default()
                ));
            }
            response.json::<Resource>().map_err(|error| {
                format!("Approval service returned invalid JSON ({status}): {error}")
            })
        })();
        let revoke = self.revoke_credential(&credential.resource_path);
        let decided = decided?;
        revoke?;
        let outcome = decided
            .spec
            .get("outcome")
            .and_then(Value::as_str)
            .unwrap_or("completed");
        self.update_link_metadata(
            &delivery,
            &delivery_link,
            merge_metadata(&delivery_link.metadata, "outcome", outcome.into()),
        )?;
        self.edit_approval_message(config, callback_message, outcome)
    }

    fn issue_user_credential(&self, user_path: &str) -> Result<IssuedCredential, String> {
        let expires_at = (Utc::now() + chrono::Duration::minutes(2)).to_rfc3339();
        self.client
            .post(format!("{}/credentials/issue", self.kas_api))
            .bearer_auth(&self.kas_token)
            .json(&json!({
                "subject": user_path,
                "expires_at": expires_at
            }))
            .send()
            .and_then(reqwest::blocking::Response::error_for_status)
            .and_then(reqwest::blocking::Response::json)
            .map_err(|error| {
                format!("could not issue temporary Credential for {user_path}: {error}")
            })
    }

    fn revoke_credential(&self, credential_path: &str) -> Result<(), String> {
        self.client
            .post(format!("{}/credentials/revoke", self.kas_api))
            .bearer_auth(&self.kas_token)
            .json(&json!({ "path": credential_path }))
            .send()
            .and_then(reqwest::blocking::Response::error_for_status)
            .map(|_| ())
            .map_err(|error| {
                format!("could not revoke temporary Credential {credential_path}: {error}")
            })
    }

    fn answer_callback(
        &self,
        config: &TelegramConfig,
        callback_query_id: &str,
        text: &str,
        show_alert: bool,
    ) -> Result<(), String> {
        let _: bool = self.telegram_call(
            config,
            "answerCallbackQuery",
            json!({
                "callback_query_id": callback_query_id,
                "text": text,
                "show_alert": show_alert
            }),
        )?;
        Ok(())
    }

    fn edit_approval_message(
        &self,
        config: &TelegramConfig,
        message: &TelegramMessage,
        outcome: &str,
    ) -> Result<(), String> {
        let original = message.text.as_deref().unwrap_or("KAS Approval");
        let _: TelegramMessage = self.telegram_call(
            config,
            "editMessageText",
            json!({
                "chat_id": message.chat.id,
                "message_id": message.message_id,
                "text": format!("{original}\n\nResult: {outcome}"),
                "reply_markup": {
                    "inline_keyboard": []
                }
            }),
        )?;
        Ok(())
    }

    fn send_private_text(
        &self,
        config: &TelegramConfig,
        chat_id: i64,
        text: &str,
    ) -> Result<(), String> {
        let _: TelegramMessage = self.telegram_call(
            config,
            "sendMessage",
            json!({
                "chat_id": chat_id,
                "text": text
            }),
        )?;
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

    fn update_link_metadata(
        &self,
        resource: &Resource,
        link: &LinkSpec,
        metadata: Value,
    ) -> Result<(), String> {
        let response = self
            .client
            .patch(format!("{}/resources/by-path", self.kas_api))
            .bearer_auth(&self.kas_token)
            .query(&[("path", resource.path.as_str())])
            .json(&json!({
                "expected_revision": resource.revision,
                "spec": {
                    "relation": link.relation,
                    "source": link.source,
                    "target": link.target,
                    "metadata": metadata
                }
            }))
            .send()
            .map_err(|error| format!("could not update {}: {error}", resource.path))?;
        response
            .error_for_status()
            .map(|_| ())
            .map_err(|error| format!("could not update {}: {error}", resource.path))
    }

    fn delete_resource(&self, resource: &Resource) -> Result<(), String> {
        let revision = resource.revision.to_string();
        let response = self
            .client
            .delete(format!("{}/resources/by-path", self.kas_api))
            .bearer_auth(&self.kas_token)
            .query(&[
                ("path", resource.path.as_str()),
                ("expected_revision", revision.as_str()),
            ])
            .send()
            .map_err(|error| format!("could not delete {}: {error}", resource.path))?;
        response
            .error_for_status()
            .map(|_| ())
            .map_err(|error| format!("could not delete {}: {error}", resource.path))
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

    fn send_attachment(
        &self,
        config: &TelegramConfig,
        topic_id: i64,
        reply_message_id: Option<i64>,
        file: &Resource,
    ) -> Result<TelegramMessage, String> {
        let spec: FileSpec = serde_json::from_value(file.spec.clone())
            .map_err(|error| format!("invalid File {}: {error}", file.path))?;
        let response = self
            .client
            .get(format!("{}/files/content", self.file_api))
            .bearer_auth(&self.kas_token)
            .query(&[("path", file.path.as_str())])
            .send()
            .map_err(|error| format!("could not download File {}: {error}", file.path))?
            .error_for_status()
            .map_err(|error| format!("could not download File {}: {error}", file.path))?;
        let content = response
            .bytes()
            .map_err(|error| format!("could not read File {}: {error}", file.path))?;
        let (method, field) = telegram_media_method(&spec.media_type);
        let part = multipart::Part::bytes(content.to_vec())
            .file_name(spec.filename)
            .mime_str(&spec.media_type)
            .map_err(|error| format!("invalid File media type {}: {error}", spec.media_type))?;
        let mut form = multipart::Form::new()
            .text("chat_id", config.chat_id.clone())
            .text("message_thread_id", topic_id.to_string())
            .part(field, part);
        if let Some(reply) = reply_message_id {
            form = form.text(
                "reply_parameters",
                json!({ "message_id": reply }).to_string(),
            );
        }
        let base = config.api_base.trim_end_matches('/');
        let response = self
            .client
            .post(format!("{base}/bot{}/{method}", config.bot_token))
            .multipart(form)
            .send()
            .map_err(|error| format!("Telegram {method} failed: {error}"))?;
        let status = response.status();
        let response: TelegramResponse<TelegramMessage> = response.json().map_err(|error| {
            format!("Telegram {method} returned invalid JSON ({status}): {error}")
        })?;
        if !response.ok {
            return Err(format!(
                "Telegram {method} failed ({status}): {}",
                response
                    .description
                    .unwrap_or_else(|| "ok=false without a description".into())
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
            let bot: TelegramUser = self.telegram_call(&config, "getMe", json!({}))?;
            let bot_username = bot
                .username
                .filter(|username| !username.trim().is_empty())
                .ok_or_else(|| "Telegram Bot has no username".to_owned())?;
            if config.bot_username.as_deref() != Some(bot_username.as_str()) {
                let mut spec = resource.spec.as_object().cloned().ok_or_else(|| {
                    format!(
                        "Telegram configuration {} spec is not an object",
                        resource.path
                    )
                })?;
                spec.insert("bot_username".into(), bot_username.into());
                return Ok(vec![Mutation::UpdateResource {
                    resource_path: resource.path.clone(),
                    expected_revision: resource.revision,
                    metadata: None,
                    spec: Value::Object(spec),
                }]);
            }
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
            if link.relation == MESSAGE_THREAD || link.relation == ATTACHED_TO {
                let message_path = if link.relation == MESSAGE_THREAD {
                    &link.source
                } else {
                    &link.target
                };
                return match self.get_resource(message_path)? {
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
            let existing_copy = links.iter().find_map(|link_resource| {
                let link = serde_json::from_value::<LinkSpec>(link_resource.spec.clone()).ok()?;
                (link_resource.metadata.state != kas_core::STATE_DELETED
                    && link.relation == MESSAGE_COPY
                    && link.source == resource.path
                    && link.target == configuration.path)
                    .then_some((link_resource, link))
            });
            let mut attachment_paths = links
                .iter()
                .filter_map(|link_resource| {
                    let link =
                        serde_json::from_value::<LinkSpec>(link_resource.spec.clone()).ok()?;
                    (link_resource.metadata.state != kas_core::STATE_DELETED
                        && link.relation == ATTACHED_TO
                        && link.target == resource.path)
                        .then_some(link.source)
                })
                .collect::<Vec<_>>();
            attachment_paths.sort();
            attachment_paths.dedup();
            let mut delivered_attachments = existing_copy
                .as_ref()
                .and_then(|(_, link)| link.metadata.get("attachment_paths"))
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect::<HashSet<_>>();
            let pending_attachments = attachment_paths
                .iter()
                .filter(|path| !delivered_attachments.contains(*path))
                .cloned()
                .collect::<Vec<_>>();
            if existing_copy.is_some() && pending_attachments.is_empty() {
                continue;
            }
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
            if existing_copy.is_none() && text.trim().is_empty() && pending_attachments.is_empty() {
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
            let mut message_ids = existing_copy
                .as_ref()
                .and_then(|(_, link)| link.metadata.get("message_ids"))
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_i64)
                .collect::<Vec<_>>();
            if existing_copy.is_none() {
                for chunk in split_telegram_text(&text) {
                    let mut request = json!({
                        "chat_id": config.chat_id,
                        "message_thread_id": topic_id,
                        "text": chunk
                    });
                    if let Some(reply) = reply_message_id {
                        request["reply_parameters"] = json!({ "message_id": reply });
                    }
                    let sent: TelegramMessage =
                        self.telegram_call(&config, "sendMessage", request)?;
                    message_ids.push(sent.message_id);
                }
            }
            for attachment_path in pending_attachments {
                let file = self
                    .get_resource(&attachment_path)?
                    .ok_or_else(|| format!("attached File {attachment_path} does not exist"))?;
                if file.manifest != FILE_MANIFEST || file.metadata.state == kas_core::STATE_DELETED
                {
                    return Err(format!(
                        "attachment {attachment_path} is not an available File"
                    ));
                }
                let sent = self.send_attachment(&config, topic_id, reply_message_id, &file)?;
                message_ids.push(sent.message_id);
                delivered_attachments.insert(attachment_path);
            }
            if message_ids.is_empty() {
                continue;
            }
            let mut delivered_attachments = delivered_attachments.into_iter().collect::<Vec<_>>();
            delivered_attachments.sort();
            let metadata = json!({
                "direction": "kas-to-telegram",
                "message_ids": message_ids,
                "attachment_paths": delivered_attachments
            });
            if let Some((copy_resource, copy)) = existing_copy {
                mutations.push(Mutation::UpdateResource {
                    resource_path: copy_resource.path.clone(),
                    expected_revision: copy_resource.revision,
                    metadata: None,
                    spec: serde_json::to_value(LinkSpec {
                        relation: copy.relation,
                        source: copy.source,
                        target: copy.target,
                        metadata,
                    })
                    .expect("LinkSpec is serializable"),
                });
            } else {
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
                        metadata,
                    ),
                });
            }
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

fn telegram_media_method(media_type: &str) -> (&'static str, &'static str) {
    if media_type == "image/gif" {
        ("sendAnimation", "animation")
    } else if media_type.starts_with("image/") {
        ("sendPhoto", "photo")
    } else if media_type.starts_with("video/") {
        ("sendVideo", "video")
    } else if media_type.starts_with("audio/") {
        ("sendAudio", "audio")
    } else {
        ("sendDocument", "document")
    }
}

fn hash_token(token: &str) -> String {
    Sha256::digest(token.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn approval_message(approval: &Resource) -> String {
    let reason = approval
        .spec
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or("Approval requested");
    let operation = approval
        .spec
        .get("operation")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let operation = serde_json::to_string_pretty(&operation).unwrap_or_else(|_| "{}".into());
    let expires_at = approval
        .spec
        .get("expires_at")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let message = format!(
        "KAS Approval\n\nReason: {reason}\nExpires: {expires_at}\n\nOperation:\n{operation}"
    );
    message.chars().take(3900).collect()
}

fn merge_metadata(metadata: &Value, key: &str, value: Value) -> Value {
    let mut metadata = metadata.as_object().cloned().unwrap_or_default();
    metadata.insert(key.into(), value);
    Value::Object(metadata)
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

    #[test]
    fn attachments_use_the_matching_telegram_method() {
        assert_eq!(telegram_media_method("image/png"), ("sendPhoto", "photo"));
        assert_eq!(
            telegram_media_method("image/gif"),
            ("sendAnimation", "animation")
        );
        assert_eq!(telegram_media_method("video/mp4"), ("sendVideo", "video"));
        assert_eq!(telegram_media_method("audio/mpeg"), ("sendAudio", "audio"));
        assert_eq!(
            telegram_media_method("application/zip"),
            ("sendDocument", "document")
        );
    }
}
