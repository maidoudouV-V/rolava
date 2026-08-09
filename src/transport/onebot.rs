use crate::config::AppConfig;
use crate::conversation_trigger::{ConversationTrigger, RoutedConversationTrigger};
use crate::repository::db_manager::{NewChatMessage, QQChatContextManager};
use crate::transport::message::{
    Conversation, ConversationKind, IncomingMessage, MessageContent, MessagePart, MessageTarget,
    Participant,
};
use crate::transport::{GroupInfo, MessageSender, QqExpression, SendOptions, SentMessage};
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
use tokio::sync::{mpsc, oneshot};
use tokio::time::{sleep, Duration};
use tracing::{debug, error, info, trace, warn};

mod history;

const RAW_MESSAGE_CHANNEL_CAPACITY: usize = 128;
const FOLLOWUP_SEGMENT_DELAY_PER_CHAR_MS: u64 = 100;
const FOLLOWUP_SEGMENT_MAX_DELAY: Duration = Duration::from_secs(6);
const QQ_RANDOM_EXPRESSION_ANIMATION_DELAY: Duration = Duration::from_secs(3);

/// OneBot 原始上报事件 DTO。
/// 直接对应 OneBot 的 HTTP 上报 JSON，通过 `post_type` 区分事件大类。
#[derive(Clone, Deserialize, Debug)]
#[serde(tag = "post_type")]
pub enum OneBotEventDto {
    /// 消息事件，上报私聊或群聊消息。
    #[serde(rename = "message")]
    Message(OneBotMessageEnvelopeDto),
    /// 通知事件，例如群成员变动、撤回和头像戳一戳。
    #[serde(rename = "notice")]
    Notice(OneBotNoticeEventDto),
    /// 元事件，上报生命周期或心跳。
    #[serde(rename = "meta_event")]
    Meta(OneBotMetaEventDto),
}

/// OneBot 通知事件；当前只消费目标为机器人的头像戳一戳通知。
#[derive(Clone, Deserialize, Debug)]
pub struct OneBotNoticeEventDto {
    /// 事件发生时间戳，单位秒。
    pub time: i64,
    /// 当前机器人的 QQ 号。
    pub self_id: i64,
    /// 通知类型；戳一戳在 NapCat 中为 `notify`。
    pub notice_type: String,
    /// 通知子类型；头像戳一戳为 `poke`。
    #[serde(default)]
    pub sub_type: String,
    /// 发起戳一戳的用户 QQ 号。
    pub user_id: Option<i64>,
    /// 被戳用户 QQ 号。
    pub target_id: Option<i64>,
    /// 群聊通知携带群号，私聊通知不携带。
    pub group_id: Option<i64>,
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

impl OneBotNoticeEventDto {
    /// 将目标为机器人的头像戳一戳转换为会话目标，其他通知直接忽略。
    fn poke_trigger_target(&self) -> Option<(MessageTarget, i64)> {
        if self.notice_type != "notify"
            || self.sub_type != "poke"
            || self.target_id != Some(self.self_id)
        {
            return None;
        }

        let user_id = self.user_id?;
        let (conversation_id, conversation_kind) = match self.group_id {
            Some(group_id) => (group_id.to_string(), ConversationKind::Group),
            None => (user_id.to_string(), ConversationKind::Direct),
        };
        Some((
            MessageTarget {
                source: "onebot".to_string(),
                bot_id: self.self_id.to_string(),
                conversation: Conversation {
                    id: conversation_id,
                    kind: conversation_kind,
                    title: None,
                },
            },
            user_id,
        ))
    }
}

impl OneBotMessageSegmentDto {
    /// 将 OneBot 消息段数组渲染成适合写入聊天记录和提示词的临时文本。
    fn render_message_text(
        segments: &[Self],
        bot_id: i64,
        at_display_names: &HashMap<String, String>,
        face_id_map: &HashMap<String, String>,
    ) -> String {
        let mut text = String::new();
        for segment in segments {
            segment.push_display_text(&mut text, bot_id, at_display_names, face_id_map);
        }
        text.trim().to_string()
    }

    /// 将单个消息段追加到展示文本中；非文本段用简短占位符表达。
    fn push_display_text(
        &self,
        output: &mut String,
        bot_id: i64,
        at_display_names: &HashMap<String, String>,
        face_id_map: &HashMap<String, String>,
    ) {
        match self.type_.as_str() {
            "text" => Self::push_text(
                output,
                self.data_string("text").as_deref().unwrap_or_default(),
            ),
            "at" => Self::push_token(output, &self.render_at_text(bot_id, at_display_names)),
            "face" => Self::push_token(output, &self.face_context_text(face_id_map)),
            "image" => Self::push_token(output, "[图片]"),
            "record" => Self::push_token(output, "[语音]"),
            "video" => Self::push_token(output, "[视频]"),
            "file" => Self::push_token(output, &self.file_context_text()),
            "reply" => Self::push_token(output, "[回复消息]"),
            "forward" => Self::push_token(output, "[合并转发]"),
            "node" => Self::push_token(output, "[转发节点]"),
            "share" => Self::push_token(output, &self.render_named_placeholder("分享", "title")),
            "contact" => Self::push_token(output, "[名片]"),
            "location" => Self::push_token(output, &self.render_named_placeholder("位置", "name")),
            "music" => Self::push_token(output, "[音乐]"),
            "json" => Self::push_token(output, &Self::json_context_text(&self.data)),
            "xml" => Self::push_token(output, "[XML消息]"),
            "dice" => Self::push_token(output, &self.dice_context_text()),
            "rps" => Self::push_token(output, &self.rps_context_text()),
            "shake" => Self::push_token(output, "[窗口抖动]"),
            "poke" => Self::push_token(output, &self.poke_context_text()),
            "anonymous" => Self::push_token(output, "[匿名]"),
            other => Self::push_token(output, &format!("[{}]", other)),
        }
    }

    /// 超级表情优先使用平台文本，经典表情再按 ID 查表。
    fn face_context_text(&self, face_id_map: &HashMap<String, String>) -> String {
        let face_text = self
            .data
            .get("raw")
            .and_then(|raw| raw.get("faceText"))
            .and_then(Value::as_str)
            .or_else(|| self.data.get("faceText").and_then(Value::as_str))
            .and_then(Self::normalize_face_text)
            .or_else(|| {
                self.data_string("id")
                    .and_then(|id| face_id_map.get(&id).map(String::as_str))
                    .and_then(Self::normalize_face_text)
            });

        match face_text {
            Some(face_text) => format!("[QQ表情:{}]", face_text),
            None => "[QQ表情]".to_string(),
        }
    }

    fn normalize_face_text(text: &str) -> Option<&str> {
        let text = text.trim();
        let text = text
            .strip_prefix('[')
            .and_then(|text| text.strip_suffix(']'))
            .unwrap_or(text)
            .trim();
        (!text.is_empty()).then_some(text)
    }

    fn dice_context_text(&self) -> String {
        match self.result_text() {
            Some(result) => format!("[QQ表情:骰子(点数:{})]", result),
            None => "[QQ表情:骰子]".to_string(),
        }
    }

    fn rps_context_text(&self) -> String {
        let result = self.result_text().and_then(|result| match result.as_str() {
            "1" => Some("布"),
            "2" => Some("剪刀"),
            "3" => Some("拳头"),
            _ => None,
        });
        match result {
            Some(result) => format!("[QQ表情:包剪锤(出拳:{})]", result),
            None => "[QQ表情:包剪锤]".to_string(),
        }
    }

    /// QQ 的 poke 消息段是原生表情，不是点击头像触发的戳一戳动作。
    fn poke_context_text(&self) -> String {
        let name = self
            .data_string("type")
            .and_then(|type_| match type_.as_str() {
                "1" => Some("戳一戳"),
                "2" => Some("比心"),
                "3" => Some("点赞"),
                "4" => Some("心碎"),
                _ => None,
            });

        match name {
            Some(name) => format!("[互动表情:{}]", name),
            None => "[互动表情]".to_string(),
        }
    }

    /// 结果字段只接受非空字符串或数字，null 等结构化值视为无结果。
    fn result_text(&self) -> Option<String> {
        let value = self.data.get("result")?;
        let result = match value {
            Value::String(result) => result.trim().to_string(),
            Value::Number(result) => result.to_string(),
            _ => return None,
        };
        (!result.is_empty()).then_some(result)
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

    /// 文件段只展示平台明确提供的文件名和大小，不根据扩展名推断类型。
    fn file_context_text(&self) -> String {
        let file_name = self
            .data_string("file")
            .or_else(|| self.data_string("file_id"))
            .map(|name| name.split_whitespace().collect::<Vec<_>>().join(" "))
            .filter(|name| !name.is_empty());
        let file_size = self
            .data_string("file_size")
            .and_then(|size| size.parse::<u64>().ok())
            .map(Self::human_file_size);

        let mut details = Vec::new();
        if let Some(file_name) = file_name {
            details.push(file_name);
        }
        if let Some(file_size) = file_size {
            details.push(file_size);
        }
        if details.is_empty() {
            "[文件]".to_string()
        } else {
            format!("[文件 {}]", details.join("；"))
        }
    }

    fn human_file_size(bytes: u64) -> String {
        const KIB: f64 = 1024.0;
        const MIB: f64 = KIB * 1024.0;
        const GIB: f64 = MIB * 1024.0;
        let bytes = bytes as f64;
        if bytes >= GIB {
            format!("{:.2} GB", bytes / GIB)
        } else if bytes >= MIB {
            format!("{:.2} MB", bytes / MIB)
        } else if bytes >= KIB {
            format!("{:.2} KB", bytes / KIB)
        } else {
            format!("{} B", bytes as u64)
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
        let source = Self::json_card_field(detail, &payload, &["tag", "source", "appName"]);
        let title = Self::json_card_field(detail, &payload, &["title"]);
        let name = Self::json_card_field(detail, &payload, &["name"]);
        let desc = Self::json_card_field(detail, &payload, &["desc", "content"]);
        let address = Self::json_card_field(detail, &payload, &["address"]);
        let coordinates = detail
            .and_then(Self::json_card_coordinates)
            .or_else(|| Self::json_card_coordinates(&payload));
        let link = Self::json_card_field(
            detail,
            &payload,
            &["qqdocurl", "url", "jumpUrl", "sourceUrl"],
        )
        .map(Self::normalize_card_url);

        // JSON 卡片只保留对对话有意义的字段，并避免 prompt 重复 title 或 desc。
        let mut fields = vec![label.clone()];
        let mut seen_values = vec![Self::normalize_card_text(&label).to_lowercase()];
        Self::push_json_card_field(&mut fields, &mut seen_values, "来源", source);
        Self::push_json_card_field(&mut fields, &mut seen_values, "名称", name);
        Self::push_json_card_field(&mut fields, &mut seen_values, "标题", title);
        Self::push_json_card_field(&mut fields, &mut seen_values, "内容", desc);
        Self::push_json_card_field(&mut fields, &mut seen_values, "地址", address);
        Self::push_json_card_field(&mut fields, &mut seen_values, "坐标", coordinates);
        Self::push_json_card_field(&mut fields, &mut seen_values, "摘要", prompt);
        Self::push_json_card_field(&mut fields, &mut seen_values, "链接", link);
        format!("[{}]", fields.join("；"))
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

    /// 判断卡片类型，返回不带方括号的类型名。
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
            label.trim().to_string()
        } else if payload
            .get("app")
            .and_then(Value::as_str)
            .is_some_and(|app| app.contains("miniapp"))
        {
            "QQ小程序".to_string()
        } else {
            "JSON卡片".to_string()
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

    fn json_card_field(detail: Option<&Value>, payload: &Value, keys: &[&str]) -> Option<String> {
        detail
            .and_then(|detail| Self::json_first_string(detail, keys))
            .or_else(|| Self::json_first_string(payload, keys))
    }

    fn json_card_coordinates(value: &Value) -> Option<String> {
        let latitude = Self::json_first_string(value, &["lat", "latitude"])?;
        let longitude = Self::json_first_string(value, &["lng", "lon", "longitude"])?;
        Some(format!("{},{}", latitude, longitude))
    }

    fn json_first_string(value: &Value, keys: &[&str]) -> Option<String> {
        keys.iter().find_map(|key| Self::json_string(value, key))
    }

    /// 忽略空字段和已经出现过的文本，避免卡片摘要重复标题或正文。
    fn push_json_card_field(
        fields: &mut Vec<String>,
        seen_values: &mut Vec<String>,
        name: &str,
        value: Option<String>,
    ) {
        let Some(value) = value.map(|value| Self::normalize_card_text(&value)) else {
            return;
        };
        if value.is_empty() {
            return;
        }
        let comparison_value = value.to_lowercase();
        if seen_values.contains(&comparison_value) {
            return;
        }
        fields.push(format!("{}：{}", name, value));
        seen_values.push(comparison_value);
    }

    fn normalize_card_text(text: &str) -> String {
        text.split_whitespace().collect::<Vec<_>>().join(" ")
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
    fn into_message_part(self, face_id_map: &HashMap<String, String>) -> MessagePart {
        let context_text = match self.type_.as_str() {
            "json" => Some(Self::json_context_text(&self.data)),
            "file" => Some(self.file_context_text()),
            "face" => Some(self.face_context_text(face_id_map)),
            "dice" => Some(self.dice_context_text()),
            "rps" => Some(self.rps_context_text()),
            "poke" => Some(self.poke_context_text()),
            _ => None,
        };
        let Self {
            type_: kind,
            mut data,
        } = self;
        if let (Some(context_text), Value::Object(map)) = (context_text, &mut data) {
            map.insert("context_text".to_string(), Value::String(context_text));
        }
        MessagePart { kind, data }
    }
}

impl OneBotMessageEnvelopeDto {
    /// 转换为 transport 层通用消息结构。
    async fn into_incoming_message(self, server: &OneBotHttpServer) -> IncomingMessage {
        match self.message {
            OneBotMessageDto::Private(message) => {
                server.cache_user_display_name(message.user_id, &message.sender.nickname);
                message.into_incoming_message(
                    self.time,
                    self.self_id,
                    &HashMap::new(),
                    &server.face_id_map,
                )
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
                message.into_incoming_message(
                    self.time,
                    self.self_id,
                    &at_display_names,
                    &server.face_id_map,
                )
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
        face_id_map: &HashMap<String, String>,
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
        let text = OneBotMessageSegmentDto::render_message_text(
            &message,
            self_id,
            at_display_names,
            face_id_map,
        );
        let parts = message
            .into_iter()
            .map(|part| part.into_message_part(face_id_map))
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
        face_id_map: &HashMap<String, String>,
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
        let text = OneBotMessageSegmentDto::render_message_text(
            &message,
            self_id,
            at_display_names,
            face_id_map,
        );
        let parts = message
            .into_iter()
            .map(|part| part.into_message_part(face_id_map))
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

/// 带标准状态字段的 OneBot 动作响应。
#[derive(Deserialize, Debug)]
struct OneBotActionResponse<T> {
    status: String,
    retcode: i32,
    data: Option<T>,
    message: Option<String>,
    wording: Option<String>,
}

#[derive(Deserialize, Debug)]
struct OneBotSendMessageData {
    message_id: Option<Value>,
}

#[derive(Deserialize, Debug)]
struct OneBotGetMessageData {
    #[serde(default)]
    message: Vec<OneBotMessageSegmentDto>,
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

/// OneBot 群基础资料响应数据。
#[derive(Deserialize, Debug)]
struct OneBotGroupInfoDto {
    group_name: String,
    member_count: u64,
}

/// OneBot 用户信息响应数据。
#[derive(Deserialize, Debug)]
struct OneBotUserInfoDto {
    /// QQ 昵称。
    nickname: String,
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

    fn qq_expression_message(expression: QqExpression) -> (&'static str, Value, String) {
        match expression {
            QqExpression::Face { id, name } => (
                "face",
                serde_json::json!({ "id": id }),
                format!("[QQ表情:{}]", name),
            ),
            QqExpression::Dice => ("dice", serde_json::json!({}), "[QQ表情:骰子]".to_string()),
            QqExpression::Rps => ("rps", serde_json::json!({}), "[QQ表情:包剪锤]".to_string()),
        }
    }

    fn request_parts(
        target: &MessageTarget,
        message: Value,
    ) -> (&'static str, &'static str, Value) {
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
                    "message": message,
                }),
            ),
            ConversationKind::Group => (
                "send_group_msg",
                "group",
                serde_json::json!({
                    "group_id": target_id,
                    "message": message,
                }),
            ),
        }
    }

    fn response_error<T>(response: &OneBotActionResponse<T>) -> String {
        response
            .wording
            .as_deref()
            .or(response.message.as_deref())
            .unwrap_or("未提供错误详情")
            .to_string()
    }

    async fn send_message_request(
        &self,
        target: &MessageTarget,
        message: Value,
    ) -> Result<(&'static str, OneBotActionResponse<OneBotSendMessageData>)> {
        let (api_path, conversation_kind, payload) = Self::request_parts(target, message);
        trace!(api_path, payload = %payload, "OneBot 发送消息完整请求");
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
        trace!(status = %http_status, response = %response_text, "OneBot 发送消息原始响应");
        if !http_status.is_success() {
            bail!("OneBot 发送消息接口返回 HTTP {}", http_status);
        }

        let response: OneBotActionResponse<OneBotSendMessageData> =
            serde_json::from_str(&response_text).context("解析 OneBot 发送响应失败")?;
        if response.status != "ok" || response.retcode != 0 {
            bail!(
                "OneBot 发送消息失败，status={}，retcode={}：{}",
                response.status,
                response.retcode,
                Self::response_error(&response)
            );
        }
        Ok((conversation_kind, response))
    }

    fn response_message_id(
        response: &OneBotActionResponse<OneBotSendMessageData>,
    ) -> Option<&Value> {
        response
            .data
            .as_ref()
            .and_then(|data| data.message_id.as_ref())
    }

    fn message_id_text(message_id: &Value) -> String {
        match message_id {
            Value::String(message_id) => message_id.clone(),
            other => other.to_string(),
        }
    }

    fn persist_sent_message(
        &self,
        target: &MessageTarget,
        conversation_kind: &str,
        response: &OneBotActionResponse<OneBotSendMessageData>,
        text: &str,
        message_type: &str,
        content_parts: Value,
    ) -> Result<SentMessage> {
        let outgoing_message = NewChatMessage {
            source: target.source.clone(),
            source_conversation_id: target.conversation.id.clone(),
            conversation_kind: conversation_kind.to_string(),
            conversation_title: target.conversation.title.clone(),
            conversation_metadata_json: "{}".to_string(),
            source_message_id: Self::response_message_id(response).map(Self::message_id_text),
            sender_id: target.bot_id.clone(),
            sender_display_name: target.bot_id.clone(),
            sender_nickname: None,
            sender_role: None,
            content_text: text.to_string(),
            message_type: message_type.to_string(),
            content_parts_json: content_parts.to_string(),
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

        Ok(SentMessage {
            database_id: stored_message.id,
            text: text.to_string(),
        })
    }

    /// 查询随机表情最终结果；查询失败不能重发已经成功送达的表情。
    async fn get_sent_expression_segment(
        &self,
        message_id: &Value,
        expression_type: &str,
    ) -> Result<Option<OneBotMessageSegmentDto>> {
        let payload = serde_json::json!({ "message_id": message_id });
        trace!(payload = %payload, "OneBot 查询已发送表情完整请求");
        let mut request = self
            .client
            .post(format!("{}/get_msg", self.onebot_api_url))
            .json(&payload);
        if let Some(token) = &self.onebot_token {
            request = request.header(AUTHORIZATION, format!("Bearer {}", token));
        }

        let response = request
            .send()
            .await
            .context("调用 OneBot get_msg 接口失败")?;
        let http_status = response.status();
        let response_text = response
            .text()
            .await
            .context("读取 OneBot get_msg 响应失败")?;
        trace!(status = %http_status, response = %response_text, "OneBot get_msg 原始响应");
        if !http_status.is_success() {
            bail!("OneBot get_msg 接口返回 HTTP {}", http_status);
        }

        let response: OneBotActionResponse<OneBotGetMessageData> =
            serde_json::from_str(&response_text).context("解析 OneBot get_msg 响应失败")?;
        if response.status != "ok" || response.retcode != 0 {
            bail!(
                "OneBot get_msg 返回错误，status={}，retcode={}：{}",
                response.status,
                response.retcode,
                Self::response_error(&response)
            );
        }
        Ok(response
            .data
            .into_iter()
            .flat_map(|data| data.message)
            .find(|segment| segment.type_ == expression_type))
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
        let (conversation_kind, response) = self
            .send_message_request(target, Value::String(text.to_string()))
            .await?;

        if !persist {
            info!(
                conversation_id = %target.conversation.id,
                "OneBot 临时消息发送成功"
            );
            return Ok(None);
        }

        self.persist_sent_message(
            target,
            conversation_kind,
            &response,
            text,
            "text",
            serde_json::json!([{
                "kind": "text",
                "data": { "text": text }
            }]),
        )
        .map(Some)
    }
}

#[async_trait]
impl MessageSender for OneBotMessageSender {
    async fn get_group_info(&self, target: &MessageTarget) -> Result<Option<GroupInfo>> {
        if target.source != "onebot" || !matches!(target.conversation.kind, ConversationKind::Group)
        {
            return Ok(None);
        }

        let payload = serde_json::json!({
            "group_id": target.conversation.id,
        });
        let mut request = self
            .client
            .post(format!("{}/get_group_info", self.onebot_api_url))
            .json(&payload);
        if let Some(token) = &self.onebot_token {
            request = request.header(AUTHORIZATION, format!("Bearer {}", token));
        }

        let response = request
            .send()
            .await
            .context("调用 OneBot get_group_info 失败")?;
        let status = response.status();
        if !status.is_success() {
            bail!("OneBot get_group_info 返回 HTTP {}", status);
        }
        let result = response
            .json::<OneBotApiResponse<OneBotGroupInfoDto>>()
            .await
            .context("解析 OneBot get_group_info 响应失败")?;
        if result.retcode != 0 {
            bail!("OneBot get_group_info 返回错误码 {}", result.retcode);
        }
        Ok(result.data.map(|group| GroupInfo {
            name: group.group_name,
            member_count: group.member_count,
        }))
    }

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

    async fn send_qq_expression(
        &self,
        target: &MessageTarget,
        expression: QqExpression,
        options: SendOptions,
    ) -> Result<SentMessage> {
        if target.source != "onebot" {
            bail!("OneBot 发送器不支持消息来源：{}", target.source);
        }

        let delay = self.reply_delay(options);
        if !delay.is_zero() {
            debug!(delay_ms = delay.as_millis(), "等待 QQ 表情发送延时");
            sleep(delay).await;
        }

        let (expression_type, outgoing_data, fallback_text) =
            Self::qq_expression_message(expression);
        let message = serde_json::json!([{
            "type": expression_type,
            "data": outgoing_data.clone()
        }]);
        let (conversation_kind, response) = self.send_message_request(target, message).await?;

        let mut stored_data = outgoing_data;
        let mut context_text = fallback_text;
        let mut random_result_resolved = false;
        if expression_type != "face" {
            if let Some(message_id) = Self::response_message_id(&response) {
                match self
                    .get_sent_expression_segment(message_id, expression_type)
                    .await
                {
                    Ok(Some(segment)) => {
                        let resolved_context_text = match expression_type {
                            "dice" => segment.dice_context_text(),
                            "rps" => segment.rps_context_text(),
                            _ => context_text.clone(),
                        };
                        random_result_resolved = resolved_context_text != context_text;
                        context_text = resolved_context_text;
                        stored_data = segment.data;
                    }
                    Ok(None) => warn!(expression_type, "get_msg 未返回已发送的表情段"),
                    Err(error) => {
                        warn!(expression_type, error = %error, "查询已发送表情结果失败")
                    }
                }
            } else {
                warn!(
                    expression_type,
                    "发送响应缺少 message_id，无法查询随机表情结果"
                );
            }
        }

        if let Value::Object(data) = &mut stored_data {
            data.insert(
                "context_text".to_string(),
                Value::String(context_text.clone()),
            );
        }
        let sent_message = self.persist_sent_message(
            target,
            conversation_kind,
            &response,
            &context_text,
            expression_type,
            serde_json::json!([{
                "kind": expression_type,
                "data": stored_data
            }]),
        )?;

        if random_result_resolved {
            debug!(
                expression_type,
                delay_ms = QQ_RANDOM_EXPRESSION_ANIMATION_DELAY.as_millis(),
                "等待 QQ 随机表情播放完成"
            );
            sleep(QQ_RANDOM_EXPRESSION_ANIMATION_DELAY).await;
        }
        Ok(sent_message)
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
    /// 不入库的平台通知通过内部触发通道路由到对应会话。
    trigger_tx: mpsc::UnboundedSender<RoutedConversationTrigger>,
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
    /// 启动时加载的 QQ 经典表情名称映射。
    face_id_map: HashMap<String, String>,
}

#[derive(Clone)]
struct OneBotHttpState {
    event_tx: mpsc::Sender<OneBotEventDto>,
}

impl OneBotHttpServer {
    /// 根据应用配置创建一个 OneBot HTTP 服务实例。
    pub fn new(
        config: &AppConfig,
        message_tx: mpsc::Sender<IncomingMessage>,
        trigger_tx: mpsc::UnboundedSender<RoutedConversationTrigger>,
    ) -> Self {
        Self {
            listener_ip: config.server.server_host.clone(),
            listener_port: config.server.server_port,
            message_tx,
            trigger_tx,
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
            face_id_map: config.face_id_map.clone(),
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

    fn cached_user_display_name(&self, user_id: i64) -> Option<String> {
        self.member_name_cache
            .lock()
            .get(&Self::user_cache_key(user_id))
            .cloned()
    }

    /// 解析戳一戳发起者名称，API 查询失败时使用 QQ 号保证事件仍可触发。
    async fn resolve_poke_sender_display_name(
        &self,
        group_id: Option<i64>,
        user_id: i64,
    ) -> String {
        if let Some(group_id) = group_id {
            if let Some(display_name) = self.cached_member_display_name(group_id, user_id) {
                return display_name;
            }
            if let Some(display_name) = self
                .fetch_group_member_display_name(group_id, user_id)
                .await
            {
                self.cache_group_member_display_name(group_id, user_id, &display_name);
                return display_name;
            }
        } else {
            if let Some(display_name) = self.cached_user_display_name(user_id) {
                return display_name;
            }
            if let Some(display_name) = self.fetch_user_display_name(user_id).await {
                self.cache_user_display_name(user_id, &display_name);
                return display_name;
            }
        }

        format!("QQ {}", user_id)
    }

    /// 调用 OneBot API 获取普通用户昵称。
    async fn fetch_user_display_name(&self, user_id: i64) -> Option<String> {
        let payload = serde_json::json!({
            "user_id": user_id,
            "no_cache": false,
        });
        let mut request = self
            .client
            .post(format!("{}/get_stranger_info", self.onebot_api_url))
            .json(&payload);
        if let Some(token) = &self.onebot_token {
            request = request.header(AUTHORIZATION, format!("Bearer {}", token));
        }

        let response = match request.send().await {
            Ok(response) => response,
            Err(error) => {
                warn!(user_id, error = %error, "查询用户信息失败");
                return None;
            }
        };
        let result = match response
            .json::<OneBotApiResponse<OneBotUserInfoDto>>()
            .await
        {
            Ok(result) => result,
            Err(error) => {
                warn!(user_id, error = %error, "解析用户信息失败");
                return None;
            }
        };
        if result.retcode != 0 {
            warn!(user_id, retcode = result.retcode, "查询用户信息返回错误");
            return None;
        }

        result
            .data
            .map(|user| user.nickname.trim().to_string())
            .filter(|nickname| !nickname.is_empty())
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

    /// 将目标为机器人的戳一戳作为一次性系统提示发送到会话 Actor。
    async fn handle_notice(&self, notice: OneBotNoticeEventDto) {
        let Some((target, user_id)) = notice.poke_trigger_target() else {
            return;
        };
        let display_name = self
            .resolve_poke_sender_display_name(notice.group_id, user_id)
            .await;
        let user_prompt = format!("QQ消息：{}👉戳了戳你", display_name);
        let conversation_id = target.conversation.id.clone();
        if let Err(error) = self.trigger_tx.send(RoutedConversationTrigger {
            target,
            trigger: ConversationTrigger { user_prompt },
        }) {
            error!(conversation_id, user_id, error = %error, "戳一戳触发会话失败");
        } else {
            info!(
                conversation_id,
                user_id, "收到目标为机器人的戳一戳，已触发会话"
            );
        }
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
    pub async fn run(&self, ready_tx: oneshot::Sender<()>) {
        let listener_ip = self.listener_ip.clone();
        let listener_port = self.listener_port;
        let listener = tokio::net::TcpListener::bind(format!("{}:{}", listener_ip, listener_port))
            .await
            .unwrap();
        let log_out = format!("HTTP 服务已启动: http://{}:{}/", listener_ip, listener_port);
        let (raw_event_tx, mut raw_event_rx) =
            mpsc::channel::<OneBotEventDto>(RAW_MESSAGE_CHANNEL_CAPACITY);
        let server = Arc::new(self.clone());
        let shared_state = Arc::new(OneBotHttpState {
            event_tx: raw_event_tx,
        });
        let app = Router::new()
            .route("/", post(on_event))
            .with_state(shared_state);
        info!(address = %log_out, "OneBot HTTP 服务已启动");
        let _ = ready_tx.send(());

        let forward_events = async move {
            while let Some(event) = raw_event_rx.recv().await {
                match event {
                    OneBotEventDto::Message(message) => {
                        let incoming_message = message.into_incoming_message(&server).await;
                        if let Err(err) = server.message_tx.send(incoming_message).await {
                            error!(error = %err, "OneBot 消息发送到平台通道失败");
                            break;
                        }
                    }
                    OneBotEventDto::Notice(notice) => server.handle_notice(notice).await,
                    OneBotEventDto::Meta(_) => {}
                }
            }
        };

        tokio::select! {
            result = axum::serve(listener, app) => result.unwrap(),
            _ = forward_events => warn!("OneBot 事件转发任务已停止"),
        }
    }
}

async fn on_event(
    State(state): State<Arc<OneBotHttpState>>,
    _headers: HeaderMap,
    Json(event): Json<OneBotEventDto>,
) -> StatusCode {
    if matches!(event, OneBotEventDto::Meta(_)) {
        return StatusCode::OK;
    }
    match state.event_tx.send(event).await {
        Ok(()) => StatusCode::OK,
        Err(err) => {
            error!(error = %err, "OneBot 原始事件进入转换队列失败");
            StatusCode::SERVICE_UNAVAILABLE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{OneBotEventDto, OneBotMessageSegmentDto, OneBotMessageSender};
    use crate::transport::message::ConversationKind;
    use crate::transport::QqExpression;
    use serde_json::json;
    use std::collections::HashMap;
    use tokio::time::Duration;

    #[test]
    fn target_bot_poke_notice_resolves_group_conversation() {
        let event: OneBotEventDto = serde_json::from_value(json!({
            "time": 1786251000,
            "self_id": 644978184,
            "post_type": "notice",
            "notice_type": "notify",
            "sub_type": "poke",
            "target_id": 644978184,
            "user_id": 478931495,
            "group_id": 883510605
        }))
        .expect("戳一戳通知应当能够解析");

        let OneBotEventDto::Notice(notice) = event else {
            panic!("事件应当解析为 notice");
        };
        let (target, user_id) = notice
            .poke_trigger_target()
            .expect("目标为机器人时应当产生会话触发目标");
        assert_eq!(user_id, 478931495);
        assert_eq!(target.bot_id, "644978184");
        assert_eq!(target.conversation.id, "883510605");
        assert!(matches!(target.conversation.kind, ConversationKind::Group));
    }

    #[test]
    fn poke_notice_for_another_target_is_ignored() {
        let event: OneBotEventDto = serde_json::from_value(json!({
            "time": 1786251000,
            "self_id": 644978184,
            "post_type": "notice",
            "notice_type": "notify",
            "sub_type": "poke",
            "target_id": 123456789,
            "user_id": 478931495,
            "group_id": 883510605
        }))
        .expect("戳一戳通知应当能够解析");

        let OneBotEventDto::Notice(notice) = event else {
            panic!("事件应当解析为 notice");
        };
        assert!(notice.poke_trigger_target().is_none());
    }

    #[test]
    fn outgoing_dice_uses_onebot_message_segment() {
        let (expression_type, data, context_text) =
            OneBotMessageSender::qq_expression_message(QqExpression::Dice);
        let message = json!([{
            "type": expression_type,
            "data": data
        }]);

        assert_eq!(message, json!([{"type": "dice", "data": {}}]));
        assert_eq!(context_text, "[QQ表情:骰子]");
    }

    #[test]
    fn face_context_prefers_platform_text_then_uses_id_map() {
        let face_id_map = HashMap::from([
            ("14".to_string(), "微笑".to_string()),
            ("429".to_string(), "映射中的旧名称".to_string()),
        ]);
        let segments = vec![
            OneBotMessageSegmentDto {
                type_: "face".to_string(),
                data: json!({
                    "id": "429",
                    "raw": { "faceText": "[蛇年快乐]" }
                }),
            },
            OneBotMessageSegmentDto {
                type_: "face".to_string(),
                data: json!({
                    "id": "14",
                    "raw": { "faceText": null }
                }),
            },
        ];

        assert_eq!(
            OneBotMessageSegmentDto::render_message_text(
                &segments,
                0,
                &HashMap::new(),
                &face_id_map
            ),
            "[QQ表情:蛇年快乐] [QQ表情:微笑]"
        );
    }

    #[test]
    fn face_context_uses_generic_fallback_for_unknown_id() {
        let face = OneBotMessageSegmentDto {
            type_: "face".to_string(),
            data: json!({ "id": "999999", "raw": { "faceText": null } }),
        };

        assert_eq!(face.face_context_text(&HashMap::new()), "[QQ表情]");
        assert_eq!(
            face.into_message_part(&HashMap::new())
                .data
                .get("context_text")
                .and_then(serde_json::Value::as_str),
            Some("[QQ表情]")
        );
    }

    #[test]
    fn dice_context_keeps_point_and_omits_missing_result() {
        let segments = vec![
            OneBotMessageSegmentDto {
                type_: "dice".to_string(),
                data: json!({ "result": "2" }),
            },
            OneBotMessageSegmentDto {
                type_: "dice".to_string(),
                data: json!({ "result": null }),
            },
        ];

        assert_eq!(
            OneBotMessageSegmentDto::render_message_text(
                &segments,
                0,
                &HashMap::new(),
                &HashMap::new()
            ),
            "[QQ表情:骰子(点数:2)] [QQ表情:骰子]"
        );
    }

    #[test]
    fn rps_context_maps_results_and_omits_unknown_result() {
        let expected = [
            (json!({ "result": "1" }), "[QQ表情:包剪锤(出拳:布)]"),
            (json!({ "result": "2" }), "[QQ表情:包剪锤(出拳:剪刀)]"),
            (json!({ "result": 3 }), "[QQ表情:包剪锤(出拳:拳头)]"),
            (json!({ "result": null }), "[QQ表情:包剪锤]"),
            (json!({ "result": "4" }), "[QQ表情:包剪锤]"),
        ];

        for (data, expected_text) in expected {
            let segment = OneBotMessageSegmentDto {
                type_: "rps".to_string(),
                data,
            };
            assert_eq!(segment.rps_context_text(), expected_text);
        }
    }

    #[test]
    fn poke_context_maps_supported_types_and_uses_generic_fallback() {
        let expected = [
            (json!({ "id": "1", "type": "1" }), "[互动表情:戳一戳]"),
            (json!({ "id": "1", "type": 2 }), "[互动表情:比心]"),
            (json!({ "id": "1", "type": "3" }), "[互动表情:点赞]"),
            (json!({ "id": "1", "type": "4" }), "[互动表情:心碎]"),
            (json!({ "id": "1", "type": "5" }), "[互动表情]"),
            (json!({ "id": "1" }), "[互动表情]"),
        ];

        for (data, expected_text) in expected {
            let segment = OneBotMessageSegmentDto {
                type_: "poke".to_string(),
                data,
            };
            assert_eq!(segment.poke_context_text(), expected_text);
            assert_eq!(
                segment
                    .into_message_part(&HashMap::new())
                    .data
                    .get("context_text")
                    .and_then(serde_json::Value::as_str),
                Some(expected_text)
            );
        }
    }

    #[test]
    fn file_context_uses_only_name_and_human_readable_size() {
        let video_file = OneBotMessageSegmentDto {
            type_: "file".to_string(),
            data: json!({
                "file": "2026-08-01 12-25-48.mp4",
                "file_id": "/1678934d-10fd-400f-9b8d-857f443a2c5b",
                "file_size": "6883779",
                "url": "https://example.com/download"
            }),
        };
        let image_file = OneBotMessageSegmentDto {
            type_: "file".to_string(),
            data: json!({
                "file": "Image_1785858162735_686.png",
                "file_size": "32200024"
            }),
        };

        assert_eq!(
            video_file.file_context_text(),
            "[文件 2026-08-01 12-25-48.mp4；6.56 MB]"
        );
        assert_eq!(
            image_file.file_context_text(),
            "[文件 Image_1785858162735_686.png；30.71 MB]"
        );
        assert!(!video_file.file_context_text().contains("类型"));
    }

    #[test]
    fn json_miniapp_context_is_wrapped_and_deduplicated() {
        let card = json!({
            "data": json!({
                "app": "com.tencent.miniapp_01",
                "prompt": "[QQ小程序]群演小伙来到了久违的陈导剧组",
                "meta": {
                    "detail_1": {
                        "title": "哔哩哔哩",
                        "desc": "群演小伙来到了久违的陈导剧组",
                        "qqdocurl": "https://b23.tv/EpG0s9p"
                    }
                }
            })
            .to_string()
        });

        assert_eq!(
            OneBotMessageSegmentDto::json_context_text(&card),
            "[QQ小程序；标题：哔哩哔哩；内容：群演小伙来到了久违的陈导剧组；链接：https://b23.tv/EpG0s9p]"
        );
    }

    #[test]
    fn json_share_context_keeps_source_tag_without_repeating_title() {
        let card = json!({
            "data": json!({
                "prompt": "[分享]【额尔敦草原铜锅涮·烧烤】快来试试这家餐厅",
                "meta": {
                    "news": {
                        "tag": "美团",
                        "title": "【额尔敦草原铜锅涮·烧烤】快来试试这家餐厅",
                        "desc": "【地址：成华区双庆路万象城D馆5楼515】【电话：028-81656266】",
                        "jumpUrl": "http://dpurl.cn/e9BnMkXz"
                    }
                }
            })
            .to_string()
        });

        assert_eq!(
            OneBotMessageSegmentDto::json_context_text(&card),
            "[分享；来源：美团；标题：【额尔敦草原铜锅涮·烧烤】快来试试这家餐厅；内容：【地址：成华区双庆路万象城D馆5楼515】【电话：028-81656266】；链接：http://dpurl.cn/e9BnMkXz]"
        );
    }

    #[test]
    fn json_location_context_keeps_name_address_and_coordinates() {
        let card = json!({
            "data": json!({
                "app": "com.tencent.map",
                "prompt": "[位置]成都东站",
                "meta": {
                    "Location.Search": {
                        "address": "四川省成都市成华区金沙江路",
                        "lat": "30.628963",
                        "lng": "104.140423",
                        "name": "成都东站"
                    }
                }
            })
            .to_string()
        });

        assert_eq!(
            OneBotMessageSegmentDto::json_context_text(&card),
            "[位置；名称：成都东站；地址：四川省成都市成华区金沙江路；坐标：30.628963,104.140423]"
        );
    }

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
