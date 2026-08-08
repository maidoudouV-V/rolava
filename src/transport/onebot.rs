use crate::config::AppConfig;
use crate::repository::db_manager::{NewChatMessage, QQChatContextManager};
use crate::transport::message::{
    Conversation, ConversationKind, IncomingMessage, MessageContent, MessagePart, MessageTarget,
    Participant,
};
use crate::transport::{MessageSender, SendOptions, SentMessage};
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::{routing::post, Json, Router};
use chrono::Utc;
use parking_lot::Mutex;
use rand::Rng;
use reqwest::header::AUTHORIZATION;
use reqwest::Client;
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::time::{sleep, Duration};
use tracing::{debug, error, info, warn};

const RAW_MESSAGE_CHANNEL_CAPACITY: usize = 128;
const FOLLOWUP_SEGMENT_DELAY_PER_CHAR_MS: u64 = 100;
const FOLLOWUP_SEGMENT_MAX_DELAY: Duration = Duration::from_secs(6);

/// OneBot 原始上报事件 DTO。
/// 直接对应 OneBot 的 HTTP 上报 JSON，通过 `post_type` 区分事件大类。
#[derive(Clone, Deserialize, Debug)]
#[serde(tag = "post_type")]
pub enum OneBotEventDto {
    /// 消息事件，上报私聊或群聊消息。
    #[serde(rename = "message")]
    Message(OneBotMessageEnvelopeDto),
    /// 元事件，上报生命周期或心跳。
    #[serde(rename = "meta_event")]
    Meta(OneBotMetaEventDto),
}

/// OneBot 消息事件外层信封。
/// 公共字段在这一层，具体是私聊还是群聊由 `message_type` 决定。
#[derive(Clone, Deserialize, Debug)]
pub struct OneBotMessageEnvelopeDto {
    /// 事件发生时间戳，单位秒。
    pub time: i64,
    /// 当前机器人的 QQ 号。
    pub self_id: i64,
    /// 具体消息内容，使用 `message_type` 继续区分私聊和群聊。
    #[serde(flatten)]
    pub message: OneBotMessageDto,
}

/// OneBot 消息体 DTO。
/// 通过 `message_type` 判断是私聊消息还是群聊消息。
#[derive(Clone, Deserialize, Debug)]
#[serde(tag = "message_type")]
pub enum OneBotMessageDto {
    /// 私聊消息。
    #[serde(rename = "private")]
    Private(OneBotPrivateMessageDto),
    /// 群聊消息。
    #[serde(rename = "group")]
    Group(OneBotGroupMessageDto),
}

/// OneBot 私聊消息 DTO。
#[derive(Clone, Deserialize, Debug)]
pub struct OneBotPrivateMessageDto {
    /// 私聊子类型，常见值如 `friend`、`group`。
    pub sub_type: String,
    /// 消息 ID。
    pub message_id: i32,
    /// 发送者 QQ 号。
    pub user_id: i64,
    /// 数组格式的消息段。
    pub message: Vec<OneBotMessageSegmentDto>,
    /// 原始消息文本，保留 OneBot 上报的 CQ 码格式，便于后续调试或补充解析。
    pub raw_message: String,
    /// 字体编号。
    pub font: i32,
    /// 发送者信息。
    pub sender: OneBotPrivateSenderDto,
}

/// OneBot 私聊发送者 DTO。
#[derive(Clone, Deserialize, Debug)]
pub struct OneBotPrivateSenderDto {
    /// 发送者 QQ 号。
    pub user_id: i64,
    /// 发送者昵称。
    pub nickname: String,
    /// 性别。
    pub sex: Option<String>,
    /// 年龄。
    pub age: Option<i32>,
}

/// OneBot 群聊消息 DTO。
#[derive(Clone, Deserialize, Debug)]
pub struct OneBotGroupMessageDto {
    /// 群聊子类型，常见值如 `normal`、`notice`。
    pub sub_type: String,
    /// 消息 ID。
    pub message_id: i32,
    /// 群号。
    pub group_id: i64,
    /// 发送者 QQ 号。
    pub user_id: i64,
    /// 数组格式的消息段。
    pub message: Vec<OneBotMessageSegmentDto>,
    /// 原始消息文本，保留 OneBot 上报的 CQ 码格式，便于后续调试或补充解析。
    pub raw_message: String,
    /// 字体编号。
    pub font: i32,
    /// 发送者信息。
    pub sender: OneBotGroupSenderDto,
}

/// OneBot 群聊发送者 DTO。
#[derive(Clone, Deserialize, Debug)]
pub struct OneBotGroupSenderDto {
    /// 发送者 QQ 号。
    pub user_id: i64,
    /// 发送者昵称。
    pub nickname: String,
    /// 群名片／备注。
    pub card: Option<String>,
    /// 性别。
    pub sex: Option<String>,
    /// 年龄。
    pub age: Option<i32>,
    /// 地区。
    pub area: Option<String>,
    /// 成员等级。
    pub level: Option<String>,
    /// 群角色，如 owner / admin / member。
    pub role: Option<String>,
    /// 专属头衔。
    pub title: Option<String>,
}

/// OneBot 数组格式消息段 DTO。
#[derive(Clone, Deserialize, Debug)]
pub struct OneBotMessageSegmentDto {
    /// 消息段类型，例如 `text`、`image`、`at`。
    #[serde(rename = "type")]
    pub type_: String,
    /// 消息段参数，保持原始 JSON 结构，后续再按类型细分。
    pub data: Value,
}

/// OneBot 元事件 DTO。
#[derive(Clone, Deserialize, Debug)]
pub struct OneBotMetaEventDto {
    /// 事件发生时间戳，单位秒。
    pub time: i64,
    /// 当前机器人的 QQ 号。
    pub self_id: i64,
    /// 元事件类型，如 `lifecycle`、`heartbeat`。
    pub meta_event_type: String,
    #[serde(default)]
    /// 元事件子类型，如 `enable`、`disable`、`connect`。
    pub sub_type: String,
    /// 状态信息，通常心跳事件会带上。
    pub status: Option<OneBotMetaStatusDto>,
    #[serde(default)]
    /// 到下一次心跳的间隔，单位毫秒。
    pub interval: i64,
}

/// OneBot 元事件状态 DTO。
#[derive(Clone, Deserialize, Debug)]
pub struct OneBotMetaStatusDto {
    /// 当前 QQ 是否在线。
    pub online: bool,
    /// 当前整体运行状态是否正常。
    pub good: bool,
}

impl OneBotMessageSegmentDto {
    /// 将 OneBot 消息段数组渲染成适合写入聊天记录和提示词的临时文本。
    fn render_message_text(
        segments: &[Self],
        bot_id: i64,
        at_display_names: &HashMap<String, String>,
    ) -> String {
        let mut text = String::new();
        for segment in segments {
            segment.push_display_text(&mut text, bot_id, at_display_names);
        }
        text.trim().to_string()
    }

    /// 将单个消息段追加到展示文本中；非文本段用简短占位符表达。
    fn push_display_text(
        &self,
        output: &mut String,
        bot_id: i64,
        at_display_names: &HashMap<String, String>,
    ) {
        match self.type_.as_str() {
            "text" => Self::push_text(
                output,
                self.data_string("text").as_deref().unwrap_or_default(),
            ),
            "at" => Self::push_token(output, &self.render_at_text(bot_id, at_display_names)),
            "face" => Self::push_token(output, "[表情]"),
            "image" => Self::push_token(output, "[图片]"),
            "record" => Self::push_token(output, "[语音]"),
            "video" => Self::push_token(output, "[视频]"),
            "file" => Self::push_token(output, "[文件]"),
            "reply" => Self::push_token(output, "[回复消息]"),
            "forward" => Self::push_token(output, "[合并转发]"),
            "node" => Self::push_token(output, "[转发节点]"),
            "share" => Self::push_token(output, &self.render_named_placeholder("分享", "title")),
            "contact" => Self::push_token(output, "[名片]"),
            "location" => Self::push_token(output, &self.render_named_placeholder("位置", "name")),
            "music" => Self::push_token(output, "[音乐]"),
            "json" => Self::push_token(output, &Self::json_context_text(&self.data)),
            "xml" => Self::push_token(output, "[XML消息]"),
            "dice" => Self::push_token(output, "[骰子]"),
            "rps" => Self::push_token(output, "[猜拳]"),
            "shake" => Self::push_token(output, "[窗口抖动]"),
            "poke" => Self::push_token(output, "[戳一戳]"),
            "anonymous" => Self::push_token(output, "[匿名]"),
            other => Self::push_token(output, &format!("[{}]", other)),
        }
    }

    /// 渲染 @ 消息段；优先使用调用方解析出的昵称，失败时退回 QQ 号。
    fn render_at_text(&self, bot_id: i64, at_display_names: &HashMap<String, String>) -> String {
        let Some(qq) = self.data_string("qq") else {
            return "@未知用户".to_string();
        };
        if qq == "all" {
            "@全体成员".to_string()
        } else if qq == bot_id.to_string() {
            "@你".to_string()
        } else if let Some(display_name) = at_display_names.get(&qq) {
            format!("@{}", display_name)
        } else {
            format!("@{}", qq)
        }
    }

    /// 部分富文本段有标题或名称时尽量保留，否则只输出通用占位符。
    fn render_named_placeholder(&self, label: &str, name_key: &str) -> String {
        match self
            .data_string(name_key)
            .filter(|name| !name.trim().is_empty())
        {
            Some(name) => format!("[{}:{}]", label, name),
            None => format!("[{}]", label),
        }
    }

    /// 将 OneBot JSON 卡片转换成主聊天 AI 容易理解的摘要。
    fn json_context_text(json_data: &Value) -> String {
        let Some(payload) = Self::parse_json_segment_payload(json_data) else {
            return "[JSON消息]".to_string();
        };

        let label = Self::json_card_label(&payload);
        let prompt = payload
            .get("prompt")
            .and_then(Value::as_str)
            .map(Self::strip_card_label)
            .filter(|text| !text.is_empty());
        let detail = Self::json_card_detail(&payload);

        let app_title = detail
            .and_then(|detail| Self::json_string(detail, "title"))
            .or_else(|| Self::json_string(&payload, "title"));
        let desc = detail
            .and_then(|detail| Self::json_string(detail, "desc"))
            .or_else(|| Self::json_string(&payload, "desc"))
            .or(prompt)
            .unwrap_or_else(|| "未提供标题".to_string());
        let link = detail
            .and_then(Self::json_card_link)
            .or_else(|| Self::json_card_link(&payload));

        let mut text = match app_title {
            Some(app_title) if !app_title.trim().is_empty() => {
                format!("{} {}：{}", label, app_title.trim(), desc.trim())
            }
            _ => format!("{} {}", label, desc.trim()),
        };
        if let Some(link) = link {
            text.push_str(&format!("\n链接：{}", link));
        }
        text
    }

    /// 解析 OneBot json 消息段里嵌套的 JSON 字符串。
    fn parse_json_segment_payload(json_data: &Value) -> Option<Value> {
        if let Some(text) = json_data.get("data").and_then(Value::as_str) {
            serde_json::from_str::<Value>(text).ok()
        } else if json_data.is_object() {
            Some(json_data.clone())
        } else {
            None
        }
    }

    /// 判断卡片类型，默认按 QQ JSON 卡片处理。
    fn json_card_label(payload: &Value) -> String {
        let prompt = payload
            .get("prompt")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim();
        if let Some(label) = prompt
            .strip_prefix('[')
            .and_then(|text| text.split_once(']').map(|(label, _)| label))
            .filter(|label| !label.trim().is_empty())
        {
            format!("[{}]", label.trim())
        } else if payload
            .get("app")
            .and_then(Value::as_str)
            .is_some_and(|app| app.contains("miniapp"))
        {
            "[QQ小程序]".to_string()
        } else {
            "[JSON卡片]".to_string()
        }
    }

    /// 去掉 prompt 前缀里的卡片类型，例如 "[QQ小程序]标题" -> "标题"。
    fn strip_card_label(text: &str) -> String {
        let text = text.trim();
        if let Some((_, rest)) = text.strip_prefix('[').and_then(|text| text.split_once(']')) {
            rest.trim().to_string()
        } else {
            text.to_string()
        }
    }

    /// 从 meta 里取第一个详情对象，优先 detail_1。
    fn json_card_detail(payload: &Value) -> Option<&Value> {
        let meta = payload.get("meta")?.as_object()?;
        if let Some(detail) = meta.get("detail_1") {
            return Some(detail);
        }
        meta.values().find(|value| value.is_object())
    }

    /// 提取卡片链接，优先使用可直接打开的分享链接。
    fn json_card_link(value: &Value) -> Option<String> {
        ["qqdocurl", "url", "jumpUrl", "sourceUrl"]
            .iter()
            .find_map(|key| Self::json_string(value, key))
            .map(Self::normalize_card_url)
    }

    /// 从 JSON 对象里读取字符串字段，兼容数字字段。
    fn json_string(value: &Value, key: &str) -> Option<String> {
        let value = value.get(key)?;
        let text = if let Some(text) = value.as_str() {
            text.to_string()
        } else if let Some(number) = value.as_i64() {
            number.to_string()
        } else if let Some(number) = value.as_u64() {
            number.to_string()
        } else {
            return None;
        };
        let text = text.trim();
        if text.is_empty() {
            None
        } else {
            Some(text.to_string())
        }
    }

    /// 补齐常见无协议链接。
    fn normalize_card_url(url: String) -> String {
        let url = url.trim();
        if url.starts_with("http://") || url.starts_with("https://") {
            url.to_string()
        } else {
            format!("https://{}", url.trim_start_matches('/'))
        }
    }

    /// 获取消息段 data 中的字符串值，兼容数字形式的字段。
    fn data_string(&self, key: &str) -> Option<String> {
        let value = self.data.get(key)?;
        if let Some(text) = value.as_str() {
            Some(text.to_string())
        } else if let Some(number) = value.as_i64() {
            Some(number.to_string())
        } else if let Some(number) = value.as_u64() {
            Some(number.to_string())
        } else {
            Some(value.to_string())
        }
    }

    /// 追加普通文本，避免和前一个占位符之间出现多余空格。
    fn push_text(output: &mut String, text: &str) {
        if output
            .chars()
            .last()
            .is_some_and(|char| char.is_whitespace())
        {
            output.push_str(text.trim_start());
        } else {
            output.push_str(text);
        }
    }

    /// 追加非文本段占位符，并在前后留出边界，避免粘连到普通文字。
    fn push_token(output: &mut String, token: &str) {
        if !output.is_empty()
            && !output
                .chars()
                .last()
                .is_some_and(|char| char.is_whitespace())
        {
            output.push(' ');
        }
        output.push_str(token);
        output.push(' ');
    }

    /// 转换为 transport 层通用消息片段。
    fn into_message_part(self) -> MessagePart {
        let mut data = self.data;
        if self.type_ == "json" {
            let context_text = Self::json_context_text(&data);
            if let Value::Object(map) = &mut data {
                map.insert("context_text".to_string(), Value::String(context_text));
            }
        }
        MessagePart {
            kind: self.type_,
            data,
        }
    }
}

impl OneBotMessageEnvelopeDto {
    /// 转换为 transport 层通用消息结构。
    async fn into_incoming_message(self, server: &OneBotHttpServer) -> IncomingMessage {
        match self.message {
            OneBotMessageDto::Private(message) => {
                server.cache_user_display_name(message.user_id, &message.sender.nickname);
                message.into_incoming_message(self.time, self.self_id, &HashMap::new())
            }
            OneBotMessageDto::Group(message) => {
                let sender_display_name = OneBotHttpServer::member_display_name(
                    &message.sender.nickname,
                    message.sender.card.as_deref(),
                );
                server.cache_group_member_display_name(
                    message.group_id,
                    message.sender.user_id,
                    &sender_display_name,
                );
                let at_display_names = server
                    .resolve_at_display_names(&message.message, self.self_id, message.group_id)
                    .await;
                message.into_incoming_message(self.time, self.self_id, &at_display_names)
            }
        }
    }
}

impl OneBotPrivateMessageDto {
    /// 将 OneBot 私聊消息转换为通用消息。
    fn into_incoming_message(
        self,
        timestamp: i64,
        self_id: i64,
        at_display_names: &HashMap<String, String>,
    ) -> IncomingMessage {
        let OneBotPrivateMessageDto {
            sub_type,
            message_id,
            user_id,
            message,
            raw_message,
            font,
            sender,
        } = self;
        let OneBotPrivateSenderDto {
            user_id: sender_user_id,
            nickname,
            sex,
            age,
        } = sender;
        let text =
            OneBotMessageSegmentDto::render_message_text(&message, self_id, at_display_names);
        let parts = message
            .into_iter()
            .map(OneBotMessageSegmentDto::into_message_part)
            .collect();

        IncomingMessage {
            source: "onebot".to_string(),
            bot_id: self_id.to_string(),
            conversation: Conversation {
                id: user_id.to_string(),
                kind: ConversationKind::Direct,
                title: Some(nickname.clone()),
            },
            sender: Participant {
                id: sender_user_id.to_string(),
                display_name: nickname,
                nickname: None,
                role: None,
            },
            content: MessageContent { text, parts },
            message_id: Some(message_id.to_string()),
            timestamp,
            metadata: serde_json::json!({
                "onebot": {
                    "message_type": "private",
                    "sub_type": sub_type,
                    "raw_message": raw_message,
                    "font": font,
                    "sender": {
                        "sex": sex,
                        "age": age
                    }
                }
            }),
        }
    }
}

impl OneBotGroupMessageDto {
    /// 将 OneBot 群聊消息转换为通用消息。
    fn into_incoming_message(
        self,
        timestamp: i64,
        self_id: i64,
        at_display_names: &HashMap<String, String>,
    ) -> IncomingMessage {
        let OneBotGroupMessageDto {
            sub_type,
            message_id,
            group_id,
            user_id: _,
            message,
            raw_message,
            font,
            sender,
        } = self;
        let OneBotGroupSenderDto {
            user_id,
            nickname,
            card,
            sex,
            age,
            area,
            level,
            role,
            title,
        } = sender;
        let text =
            OneBotMessageSegmentDto::render_message_text(&message, self_id, at_display_names);
        let parts = message
            .into_iter()
            .map(OneBotMessageSegmentDto::into_message_part)
            .collect();

        IncomingMessage {
            source: "onebot".to_string(),
            bot_id: self_id.to_string(),
            conversation: Conversation {
                id: group_id.to_string(),
                kind: ConversationKind::Group,
                title: None,
            },
            sender: Participant {
                id: user_id.to_string(),
                display_name: nickname.clone(),
                nickname: card.clone(),
                role: role.clone(),
            },
            content: MessageContent { text, parts },
            message_id: Some(message_id.to_string()),
            timestamp,
            metadata: serde_json::json!({
                "onebot": {
                    "message_type": "group",
                    "sub_type": sub_type,
                    "raw_message": raw_message,
                    "font": font,
                    "group_id": group_id,
                    "sender": {
                        "nickname": nickname,
                        "card": card,
                        "sex": sex,
                        "age": age,
                        "area": area,
                        "level": level,
                        "role": role,
                        "title": title
                    }
                }
            }),
        }
    }
}

/// OneBot 发送消息响应。
#[derive(Deserialize, Debug)]
struct OneBotSendMessageResponse {
    status: String,
    retcode: i32,
    data: Option<OneBotSendMessageData>,
    message: Option<String>,
    wording: Option<String>,
}

#[derive(Deserialize, Debug)]
struct OneBotSendMessageData {
    message_id: Option<Value>,
}

/// OneBot API 通用响应。
#[derive(Deserialize, Debug)]
struct OneBotApiResponse<T> {
    /// 状态码，0 通常表示成功。
    retcode: i32,
    /// API 返回数据。
    data: Option<T>,
}

/// OneBot 群成员信息响应数据。
#[derive(Deserialize, Debug)]
struct OneBotGroupMemberInfoDto {
    /// QQ 昵称。
    nickname: String,
    /// 群名片。
    card: Option<String>,
}

/// OneBot 出站消息发送器。平台确认发送成功后，负责写入统一聊天记录。
pub struct OneBotMessageSender {
    client: Client,
    onebot_api_url: String,
    onebot_token: Option<String>,
    db_manager: Arc<QQChatContextManager>,
    split_reply_on_newlines: bool,
    reply_delay_random_max_secs: f64,
}

impl OneBotMessageSender {
    pub fn new(config: &AppConfig, db_manager: Arc<QQChatContextManager>) -> Self {
        Self {
            client: Client::new(),
            onebot_api_url: config.server.onebot_api.clone(),
            onebot_token: if config.server.onebot_token.is_empty() {
                None
            } else {
                Some(config.server.onebot_token.clone())
            },
            db_manager,
            split_reply_on_newlines: config.app.split_reply_on_newlines,
            reply_delay_random_max_secs: config.app.reply_delay_random_max_secs,
        }
    }

    fn reply_delay(&self, options: SendOptions) -> Duration {
        let random_max_secs = self.reply_delay_random_max_secs.max(0.0);
        let total_delay_secs = if random_max_secs > 0.0 {
            rand::thread_rng().gen_range(0.0..=random_max_secs)
        } else {
            0.0
        };
        let total_delay = Duration::from_secs_f64(total_delay_secs);
        let elapsed = options
            .delay_started_at
            .map(|started_at| started_at.elapsed())
            .unwrap_or_default();

        total_delay.saturating_sub(elapsed)
    }

    /// 后续分段在随机延时上按实际字符数追加打字时间，并限制总延时最多六秒。
    fn followup_segment_delay(random_delay: Duration, text: &str) -> Duration {
        let character_count = u64::try_from(text.chars().count()).unwrap_or(u64::MAX);
        let typing_delay = Duration::from_millis(
            character_count.saturating_mul(FOLLOWUP_SEGMENT_DELAY_PER_CHAR_MS),
        );
        random_delay
            .saturating_add(typing_delay)
            .min(FOLLOWUP_SEGMENT_MAX_DELAY)
    }

    fn request_parts(target: &MessageTarget, text: &str) -> (&'static str, &'static str, Value) {
        let target_id = target
            .conversation
            .id
            .parse::<i64>()
            .map(Value::from)
            .unwrap_or_else(|_| Value::String(target.conversation.id.clone()));

        match target.conversation.kind {
            ConversationKind::Direct => (
                "send_private_msg",
                "direct",
                serde_json::json!({
                    "user_id": target_id,
                    "message": text,
                }),
            ),
            ConversationKind::Group => (
                "send_group_msg",
                "group",
                serde_json::json!({
                    "group_id": target_id,
                    "message": text,
                }),
            ),
        }
    }

    fn response_error(response: &OneBotSendMessageResponse) -> String {
        response
            .wording
            .as_deref()
            .or(response.message.as_deref())
            .unwrap_or("未提供错误详情")
            .to_string()
    }

    fn text_segments(text: &str, split_on_newlines: bool) -> Vec<&str> {
        if !split_on_newlines {
            return (!text.trim().is_empty())
                .then_some(text)
                .into_iter()
                .collect();
        }

        text.lines()
            .map(str::trim)
            .filter(|segment| !segment.is_empty())
            .collect()
    }

    async fn send_text_segment(
        &self,
        target: &MessageTarget,
        text: &str,
        persist: bool,
    ) -> Result<Option<SentMessage>> {
        let (api_path, conversation_kind, payload) = Self::request_parts(target, text);
        debug!(api_path, payload = %payload, "OneBot 发送消息请求");
        let mut request = self
            .client
            .post(format!("{}/{}", self.onebot_api_url, api_path))
            .json(&payload);
        if let Some(token) = &self.onebot_token {
            request = request.header(AUTHORIZATION, format!("Bearer {}", token));
        }

        let response = request
            .send()
            .await
            .context("调用 OneBot 发送消息接口失败")?;
        let http_status = response.status();
        let response_text = response.text().await.context("读取 OneBot 发送响应失败")?;
        debug!(status = %http_status, response = %response_text, "OneBot 发送消息原始响应");
        if !http_status.is_success() {
            bail!("OneBot 发送消息接口返回 HTTP {}", http_status);
        }

        let response: OneBotSendMessageResponse =
            serde_json::from_str(&response_text).context("解析 OneBot 发送响应失败")?;
        if response.status != "ok" || response.retcode != 0 {
            bail!(
                "OneBot 发送消息失败，status={}，retcode={}：{}",
                response.status,
                response.retcode,
                Self::response_error(&response)
            );
        }

        if !persist {
            info!(
                conversation_id = %target.conversation.id,
                "OneBot 临时消息发送成功"
            );
            return Ok(None);
        }

        let source_message_id = response
            .data
            .as_ref()
            .and_then(|data| data.message_id.as_ref())
            .map(|id| match id {
                Value::String(id) => id.clone(),
                other => other.to_string(),
            });
        let outgoing_message = NewChatMessage {
            source: target.source.clone(),
            source_conversation_id: target.conversation.id.clone(),
            conversation_kind: conversation_kind.to_string(),
            conversation_title: target.conversation.title.clone(),
            conversation_metadata_json: "{}".to_string(),
            source_message_id,
            sender_id: target.bot_id.clone(),
            sender_display_name: target.bot_id.clone(),
            sender_nickname: None,
            sender_role: None,
            content_text: text.to_string(),
            message_type: "text".to_string(),
            content_parts_json: serde_json::json!([{
                "kind": "text",
                "data": { "text": text }
            }])
            .to_string(),
            metadata_json: serde_json::json!({
                "onebot": {
                    "status": response.status,
                    "retcode": response.retcode
                }
            })
            .to_string(),
            event_timestamp: Utc::now().timestamp(),
        };
        let stored_message = self
            .db_manager
            .write_message(&outgoing_message)
            .context("消息已发送，但写入聊天记录失败")?;
        info!(
            conversation_kind,
            conversation_id = %target.conversation.id,
            message_id = stored_message.id,
            "OneBot 消息发送并入库成功"
        );

        Ok(Some(SentMessage {
            database_id: stored_message.id,
            text: text.to_string(),
        }))
    }
}

#[async_trait]
impl MessageSender for OneBotMessageSender {
    async fn send_text(
        &self,
        target: &MessageTarget,
        text: &str,
        options: SendOptions,
    ) -> Result<Vec<SentMessage>> {
        if target.source != "onebot" {
            bail!("OneBot 发送器不支持消息来源：{}", target.source);
        }
        let segments = Self::text_segments(text, self.split_reply_on_newlines);
        if segments.is_empty() {
            bail!("不能发送空消息");
        }

        let mut sent_messages = Vec::with_capacity(segments.len());
        let segment_count = segments.len();
        for (segment_index, segment) in segments.into_iter().enumerate() {
            // 第一段沿用原计时起点；后续分段重新计算随机延时和按字数增加的打字时间。
            let delay = if segment_index == 0 {
                self.reply_delay(options)
            } else {
                Self::followup_segment_delay(self.reply_delay(SendOptions::default()), segment)
            };
            if !delay.is_zero() {
                debug!(
                    segment = segment_index + 1,
                    segment_count,
                    delay_ms = delay.as_millis(),
                    "等待分段消息发送延时"
                );
                sleep(delay).await;
            }

            let sent_message = self
                .send_text_segment(target, segment, true)
                .await?
                .expect("持久化发送必须返回数据库消息");
            sent_messages.push(sent_message);
        }

        Ok(sent_messages)
    }

    async fn send_transient_text(&self, target: &MessageTarget, text: &str) -> Result<()> {
        if target.source != "onebot" {
            bail!("OneBot 发送器不支持消息来源：{}", target.source);
        }
        let segments = Self::text_segments(text, self.split_reply_on_newlines);
        if segments.is_empty() {
            bail!("不能发送空消息");
        }

        for segment in segments {
            self.send_text_segment(target, segment, false).await?;
        }
        Ok(())
    }
}

/// onebot协议 http服务端
#[derive(Clone)]
pub struct OneBotHttpServer {
    /// 本地 HTTP 服务监听地址。
    listener_ip: String,
    /// 本地 HTTP 服务监听端口。
    listener_port: u16,
    /// 标准化后的平台消息输出通道。
    message_tx: mpsc::Sender<IncomingMessage>,
    /// OneBot HTTP API 地址。
    onebot_api_url: String,
    /// 群成员展示名缓存，key 使用 group_id:user_id 或 user_id。
    member_name_cache: Arc<Mutex<HashMap<String, String>>>,
    /// 校验上报请求使用的服务端 token。
    token: Option<String>,
    /// 发送 OneBot HTTP 请求的客户端。
    client: Client,
    /// 调用 OneBot HTTP API 使用的对端 token。
    onebot_token: Option<String>,
}

#[derive(Clone)]
struct OneBotHttpState {
    message_tx: mpsc::Sender<OneBotMessageEnvelopeDto>,
}

impl OneBotHttpServer {
    /// 根据应用配置创建一个 OneBot HTTP 服务实例。
    pub fn new(config: &AppConfig, message_tx: mpsc::Sender<IncomingMessage>) -> Self {
        Self {
            listener_ip: config.server.server_host.clone(),
            listener_port: config.server.server_port,
            message_tx,
            onebot_api_url: config.server.onebot_api.clone(),
            member_name_cache: Arc::new(Mutex::new(HashMap::new())),
            client: Client::new(),
            token: if config.server.server_token.is_empty() {
                None
            } else {
                Some(config.server.server_token.clone())
            },
            onebot_token: if config.server.onebot_token.is_empty() {
                None
            } else {
                Some(config.server.onebot_token.clone())
            },
        }
    }

    /// 校验上报请求头里的 bearer token 是否匹配当前服务端配置。
    fn verify_request_token(&self, headers: &HeaderMap) -> bool {
        let Some(expected_token) = &self.token else {
            return true;
        };
        let Some(auth_value) = headers.get(AUTHORIZATION) else {
            return false;
        };
        let Ok(auth_value) = auth_value.to_str() else {
            return false;
        };
        let mut auth_parts = auth_value.split_whitespace();
        let Some(scheme) = auth_parts.next() else {
            return false;
        };
        let Some(token) = auth_parts.next() else {
            return false;
        };
        scheme.eq_ignore_ascii_case("Bearer")
            && token == expected_token
            && auth_parts.next().is_none()
    }

    /// 缓存普通用户展示名，作为群内缓存缺失时的兜底。
    fn cache_user_display_name(&self, user_id: i64, display_name: &str) {
        let display_name = display_name.trim();
        if display_name.is_empty() {
            return;
        }
        self.member_name_cache
            .lock()
            .insert(Self::user_cache_key(user_id), display_name.to_string());
    }

    /// 缓存指定群内成员展示名，并同步写入普通用户缓存。
    fn cache_group_member_display_name(&self, group_id: i64, user_id: i64, display_name: &str) {
        let display_name = display_name.trim();
        if display_name.is_empty() {
            return;
        }

        let mut cache = self.member_name_cache.lock();
        cache.insert(
            Self::group_member_cache_key(group_id, user_id),
            display_name.to_string(),
        );
        cache.insert(Self::user_cache_key(user_id), display_name.to_string());
    }

    /// 从缓存或 OneBot API 中解析消息里 @ 目标的展示名。
    async fn resolve_at_display_names(
        &self,
        segments: &[OneBotMessageSegmentDto],
        bot_id: i64,
        group_id: i64,
    ) -> HashMap<String, String> {
        let mut at_display_names = HashMap::new();
        for segment in segments {
            if segment.type_ != "at" {
                continue;
            }

            let Some(qq) = segment.data_string("qq") else {
                continue;
            };
            if qq == "all" || qq == bot_id.to_string() || at_display_names.contains_key(&qq) {
                continue;
            }

            let Ok(user_id) = qq.parse::<i64>() else {
                continue;
            };
            if let Some(display_name) = self.cached_member_display_name(group_id, user_id) {
                at_display_names.insert(qq, display_name);
                continue;
            }

            if let Some(display_name) = self
                .fetch_group_member_display_name(group_id, user_id)
                .await
            {
                self.cache_group_member_display_name(group_id, user_id, &display_name);
                at_display_names.insert(qq, display_name);
            }
        }
        at_display_names
    }

    /// 从缓存中获取群成员展示名，优先群内名片，兜底普通用户昵称。
    fn cached_member_display_name(&self, group_id: i64, user_id: i64) -> Option<String> {
        let cache = self.member_name_cache.lock();
        cache
            .get(&Self::group_member_cache_key(group_id, user_id))
            .or_else(|| cache.get(&Self::user_cache_key(user_id)))
            .cloned()
    }

    /// 调用 OneBot API 获取单个群成员信息。
    async fn fetch_group_member_display_name(&self, group_id: i64, user_id: i64) -> Option<String> {
        let payload = serde_json::json!({
            "group_id": group_id,
            "user_id": user_id,
            "no_cache": false,
        });
        let mut request = self
            .client
            .post(format!("{}/get_group_member_info", self.onebot_api_url))
            .json(&payload);

        if let Some(token) = &self.onebot_token {
            request = request.header(AUTHORIZATION, format!("Bearer {}", token));
        }

        let resp = match request.send().await {
            Ok(resp) => resp,
            Err(err) => {
                warn!(group_id, user_id, error = %err, "查询群成员信息失败");
                return None;
            }
        };
        let result = match resp
            .json::<OneBotApiResponse<OneBotGroupMemberInfoDto>>()
            .await
        {
            Ok(result) => result,
            Err(err) => {
                warn!(group_id, user_id, error = %err, "解析群成员信息失败");
                return None;
            }
        };
        if result.retcode != 0 {
            warn!(
                group_id,
                user_id,
                retcode = result.retcode,
                "查询群成员信息返回错误"
            );
            return None;
        }

        result
            .data
            .map(|member| Self::member_display_name(&member.nickname, member.card.as_deref()))
            .filter(|display_name| !display_name.trim().is_empty())
    }

    /// 优先使用群名片，群名片为空时使用 QQ 昵称。
    fn member_display_name(nickname: &str, card: Option<&str>) -> String {
        card.map(str::trim)
            .filter(|card| !card.is_empty())
            .unwrap_or_else(|| nickname.trim())
            .to_string()
    }

    /// 群成员缓存 key。
    fn group_member_cache_key(group_id: i64, user_id: i64) -> String {
        format!("group:{}:{}", group_id, user_id)
    }

    /// 普通用户缓存 key。
    fn user_cache_key(user_id: i64) -> String {
        format!("user:{}", user_id)
    }

    /// 启动接收 OneBot 上报的 HTTP 服务。
    pub async fn run(&self) {
        let listener_ip = self.listener_ip.clone();
        let listener_port = self.listener_port;
        let listener = tokio::net::TcpListener::bind(format!("{}:{}", listener_ip, listener_port))
            .await
            .unwrap();
        let log_out = format!("HTTP 服务已启动: http://{}:{}/", listener_ip, listener_port);
        let (raw_message_tx, mut raw_message_rx) =
            mpsc::channel::<OneBotMessageEnvelopeDto>(RAW_MESSAGE_CHANNEL_CAPACITY);
        let server = Arc::new(self.clone());
        let shared_state = Arc::new(OneBotHttpState {
            message_tx: raw_message_tx,
        });
        let app = Router::new()
            .route("/", post(on_event))
            .with_state(shared_state);
        info!(address = %log_out, "OneBot HTTP 服务已启动");

        let forward_messages = async move {
            while let Some(message) = raw_message_rx.recv().await {
                let incoming_message = message.into_incoming_message(&server).await;
                if let Err(err) = server.message_tx.send(incoming_message).await {
                    error!(error = %err, "OneBot 消息发送到平台通道失败");
                    break;
                }
            }
        };

        tokio::select! {
            result = axum::serve(listener, app) => result.unwrap(),
            _ = forward_messages => warn!("OneBot 消息转发任务已停止"),
        }
    }
}

async fn on_event(
    State(state): State<Arc<OneBotHttpState>>,
    _headers: HeaderMap,
    Json(event): Json<OneBotEventDto>,
) -> StatusCode {
    let OneBotEventDto::Message(message) = event else {
        return StatusCode::OK;
    };
    match state.message_tx.send(message).await {
        Ok(()) => StatusCode::OK,
        Err(err) => {
            error!(error = %err, "OneBot 原始消息进入转换队列失败");
            StatusCode::SERVICE_UNAVAILABLE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::OneBotMessageSender;
    use tokio::time::Duration;

    #[test]
    fn text_segments_skip_consecutive_and_blank_lines() {
        let segments =
            OneBotMessageSender::text_segments("  第一段  \n\n  \r\n第二段\r\n\r\n第三段  ", true);

        assert_eq!(segments, vec!["第一段", "第二段", "第三段"]);
    }

    #[test]
    fn text_segments_preserve_original_text_when_disabled() {
        let text = "第一段\n\n第二段";

        assert_eq!(OneBotMessageSender::text_segments(text, false), vec![text]);
        assert!(OneBotMessageSender::text_segments(" \n\r\n ", false).is_empty());
    }

    #[test]
    fn followup_segment_delay_adds_per_character_time_and_caps_at_six_seconds() {
        assert_eq!(
            OneBotMessageSender::followup_segment_delay(Duration::from_millis(500), "你好a"),
            Duration::from_millis(800)
        );
        assert_eq!(
            OneBotMessageSender::followup_segment_delay(Duration::from_secs(1), &"字".repeat(100)),
            Duration::from_secs(6)
        );
    }
}
