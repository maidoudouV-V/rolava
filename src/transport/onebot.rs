use std::collections::VecDeque;
use std::sync::{Arc};
use reqwest::header::AUTHORIZATION;
use serde::{Deserialize, Serialize};
use axum::{routing::post, Router, Json};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use chrono::Utc;
use parking_lot::Mutex;
use reqwest::Client;
use serde_json::Value;
use tokio::sync::Notify;
use crate::config::AppConfig;
use crate::repository::db_manager::{NewChatMessage, QQChatContextManager};
use crate::transport::message::{
    Conversation,
    ConversationKind,
    IncomingMessage,
    MessageContent,
    MessagePart,
    Participant,
};

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
    fn render_message_text(segments: &[Self], bot_id: i64) -> String {
        let mut text = String::new();
        for segment in segments {
            segment.push_display_text(&mut text, bot_id);
        }
        text.trim().to_string()
    }

    /// 将单个消息段追加到展示文本中；非文本段用简短占位符表达。
    fn push_display_text(&self, output: &mut String, bot_id: i64) {
        match self.type_.as_str() {
            "text" => Self::push_text(output, self.data_string("text").as_deref().unwrap_or_default()),
            "at" => Self::push_token(output, &self.render_at_text(bot_id)),
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
            "json" => Self::push_token(output, "[JSON消息]"),
            "xml" => Self::push_token(output, "[XML消息]"),
            "dice" => Self::push_token(output, "[骰子]"),
            "rps" => Self::push_token(output, "[猜拳]"),
            "shake" => Self::push_token(output, "[窗口抖动]"),
            "poke" => Self::push_token(output, "[戳一戳]"),
            "anonymous" => Self::push_token(output, "[匿名]"),
            other => Self::push_token(output, &format!("[{}]", other)),
        }
    }

    /// 渲染 @ 消息段；标准 OneBot at 段通常只有 qq，因此默认显示 QQ 号。
    fn render_at_text(&self, bot_id: i64) -> String {
        let Some(qq) = self.data_string("qq") else {
            return "@未知用户".to_string();
        };
        if qq == "all" {
            "@全体成员".to_string()
        } else if qq == bot_id.to_string() {
            "@你".to_string()
        } else {
            format!("@{}", qq)
        }
    }

    /// 部分富文本段有标题或名称时尽量保留，否则只输出通用占位符。
    fn render_named_placeholder(&self, label: &str, name_key: &str) -> String {
        match self.data_string(name_key).filter(|name| !name.trim().is_empty()) {
            Some(name) => format!("[{}:{}]", label, name),
            None => format!("[{}]", label),
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
        if output.chars().last().is_some_and(|char| char.is_whitespace()) {
            output.push_str(text.trim_start());
        } else {
            output.push_str(text);
        }
    }

    /// 追加非文本段占位符，并在前后留出边界，避免粘连到普通文字。
    fn push_token(output: &mut String, token: &str) {
        if !output.is_empty()
            && !output.chars().last().is_some_and(|char| char.is_whitespace())
        {
            output.push(' ');
        }
        output.push_str(token);
        output.push(' ');
    }

    /// 转换为 transport 层通用消息片段。
    fn into_message_part(self) -> MessagePart {
        MessagePart {
            kind: self.type_,
            data: self.data,
        }
    }
}

impl OneBotMessageEnvelopeDto {
    /// 转换为 transport 层通用消息结构。
    fn into_incoming_message(self) -> IncomingMessage {
        match self.message {
            OneBotMessageDto::Private(message) => message.into_incoming_message(self.time, self.self_id),
            OneBotMessageDto::Group(message) => message.into_incoming_message(self.time, self.self_id),
        }
    }
}

impl OneBotPrivateMessageDto {
    /// 将 OneBot 私聊消息转换为通用消息。
    fn into_incoming_message(self, timestamp: i64, self_id: i64) -> IncomingMessage {
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
        let text = OneBotMessageSegmentDto::render_message_text(&message, self_id);
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
            content: MessageContent {
                text,
                parts,
            },
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
    fn into_incoming_message(self, timestamp: i64, self_id: i64) -> IncomingMessage {
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
        let text = OneBotMessageSegmentDto::render_message_text(&message, self_id);
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
            content: MessageContent {
                text,
                parts,
            },
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


/// Http请求返回结果
#[derive(Serialize, Deserialize, Debug)]
pub struct OneBotHttpResult {
    /// 结果
    pub status: String,
    /// 状态码
    pub retcode: i32,
}

const DEFAULT_EVENT_BUFFER_CAPACITY: usize = 128;

/// onebot协议 http服务端
#[derive(Clone)]
pub struct OneBotHttpServer {
    /// 本地 HTTP 服务监听地址。
    listener_ip: String,
    /// 本地 HTTP 服务监听端口。
    listener_port: u16,
    /// 接收到的 OneBot 事件缓冲区。
    event_buffer: Arc<Mutex<VecDeque<OneBotEventDto>>>,
    /// 事件缓冲区最大容量。
    event_buffer_capacity: usize,
    /// 有新消息事件到达时用于唤醒等待方。
    message_notify: Arc<Notify>,
    /// OneBot HTTP API 地址。
    onebot_api_url: String,
    /// 校验上报请求使用的服务端 token。
    token: Option<String>,
    /// 发送 OneBot HTTP 请求的客户端。
    client: Client,
    /// 调用 OneBot HTTP API 使用的对端 token。
    onebot_token: Option<String>,
}
impl OneBotHttpServer {
    /// 根据应用配置创建一个 OneBot HTTP 服务实例。
    pub fn new(config: &AppConfig) -> Self {
        Self {
            listener_ip: config.server.server_host.clone(),
            listener_port: config.server.server_port,
            event_buffer: Arc::new(Mutex::new(VecDeque::new())),
            event_buffer_capacity: DEFAULT_EVENT_BUFFER_CAPACITY,
            message_notify: Arc::new(Notify::new()),
            onebot_api_url: config.server.onebot_api.clone(),
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

    /// 将新事件写入缓冲区，并在有新消息时通知等待方。
    fn push_event(&self, event: OneBotEventDto) {
        let is_message = matches!(&event, OneBotEventDto::Message(_));
        let mut event_buffer = self.event_buffer.lock();
        if event_buffer.len() >= self.event_buffer_capacity {
            event_buffer.pop_front();
        }
        event_buffer.push_back(event);
        drop(event_buffer);
        if is_message {
            self.message_notify.notify_one();
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

    /// 非阻塞地取出缓冲区里最新的一条消息事件。
    pub fn try_take_latest_message(&self) -> Option<IncomingMessage> {
        let mut event_buffer = self.event_buffer.lock();
        let latest_message_index = event_buffer
            .iter()
            .rposition(|event| matches!(event, OneBotEventDto::Message(_)))?;
        match event_buffer.remove(latest_message_index) {
            Some(OneBotEventDto::Message(message)) => Some(message.into_incoming_message()),
            _ => None,
        }
    }

    /// 异步等待下一条可用消息，并从缓冲区中取出它。
    pub async fn recv_latest_message(&self) -> IncomingMessage {
        loop {
            if let Some(message) = self.try_take_latest_message() {
                return message;
            }
            self.message_notify.notified().await;
        }
    }

    /// 返回当前缓冲区里累计的事件数量。
    pub fn buffered_event_count(&self) -> usize {
        self.event_buffer
            .lock()
            .len()
    }

    /// 启动接收 OneBot 上报的 HTTP 服务。
    pub async fn run(&self){
        let listener_ip = self.listener_ip.clone();
        let listener_port = self.listener_port;
        let listener = tokio::net::TcpListener::bind(format!("{}:{}", listener_ip, listener_port)).await.unwrap();
        let log_out = format!("HTTP 服务已启动: http://{}:{}/", listener_ip, listener_port);
        let shared_state = Arc::new(self.clone());
        let app = Router::new()
            .route("/", post(on_event))
            .with_state(shared_state);
        println!("{}",log_out);
        axum::serve(listener, app).await.unwrap();
    }

    /// 调用 OneBot HTTP API 向当前会话发送一条消息。
    pub async fn send_message(&self, incoming_message: IncomingMessage, response_msg: &String, db_manager: Arc<QQChatContextManager>) -> reqwest::Result<()> {
        let conversation_id = incoming_message.conversation.id.clone();
        let conversation_title = incoming_message.conversation.title.clone();
        let target_id = conversation_id
            .parse::<i64>()
            .map(Value::from)
            .unwrap_or_else(|_| Value::String(conversation_id.clone()));
        let (api_path, conversation_kind, payload) = match &incoming_message.conversation.kind {
            ConversationKind::Direct => (
                "send_private_msg",
                "direct",
                serde_json::json!({
                    "user_id": target_id,
                    "message": response_msg,
                }),
            ),
            ConversationKind::Group => (
                "send_group_msg",
                "group",
                serde_json::json!({
                    "group_id": target_id,
                    "message": response_msg,
                }),
            ),
            ConversationKind::Channel => {
                eprintln!("暂不支持向频道会话发送消息: {}", conversation_id);
                return Ok(());
            }
        };
        let mut request = self.client
            .post(format!("{}/{}", self.onebot_api_url, api_path))
            .json(&payload);

        if let Some(token) = &self.onebot_token {
            request = request.header(AUTHORIZATION, format!("Bearer {}", token));
        }

        let resp = request.send().await?;
        let _resp = resp.json::<OneBotHttpResult>().await?;

        let outgoing_message = NewChatMessage {
            source: "onebot".to_string(),
            source_conversation_id: conversation_id,
            conversation_kind: conversation_kind.to_string(),
            conversation_title,
            conversation_metadata_json: "{}".to_string(),
            source_message_id: None,
            sender_id: incoming_message.bot_id.to_string(),
            sender_display_name: incoming_message.bot_id.to_string(),
            sender_nickname: None,
            sender_role: None,
            content_text: response_msg.clone(),
            message_type: "text".to_string(),
            content_parts_json: serde_json::json!([
                {
                    "kind": "text",
                    "data": {
                        "text": response_msg
                    }
                }
            ]).to_string(),
            metadata_json: "{}".to_string(),
            event_timestamp: Utc::now().timestamp(),
        };
        db_manager.write_message(&outgoing_message).unwrap();

        Ok(())
    }
}

async fn on_event(
    State(state): State<Arc<OneBotHttpServer>>,
    _headers: HeaderMap,
    Json(event): Json<OneBotEventDto>,
) -> StatusCode {
    state.push_event(event);
    StatusCode::OK
}
